#define _POSIX_C_SOURCE 200809L
/* A real CLIPRDR client restricted to known disposable test strings and images. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <poll.h>
#include <unistd.h>
#include <freerdp/freerdp.h>
#include <freerdp/client/channels.h>
#include <freerdp/client/cliprdr.h>
#include <freerdp/settings.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/error.h>
#include <winpr/synch.h>
#include <winpr/sysinfo.h>

typedef struct {
    rdpContext context;
    CliprdrClientContext *clip;
    BOOL received, sent, failed, initialized;
    UINT32 requested;
    int rich_step; /* 0: awaiting the native image, 1: its HTML, 2: done. */
} Probe;
static const UINT16 native_text[] = {'W','a','y','l','a','n','d',' ',0x65e5,0x672c,0xd83d,0xde00,'\r','\n','l','i','n','e',' ', '2',0};
static const UINT16 remote_text[] = {'R','D','P',' ',0x65e5,0x672c,0xd83d,0xde00,'\r','\n','l','i','n','e',' ', '2',0};
static const UINT16 empty_text[] = {0};
static const UINT16 intermediate_text[] = {'s','t','a','l','e',0};
/* Rich mode: fixed images and HTML matching scripts/clipboard-integration.py. */
#define CLIENT_HTML_ID 0xC0F3u
static const BYTE native_rgba[6][4] = {{255, 0, 0, 255}, {0, 255, 0, 255}, {0, 0, 255, 128},
                                       {10, 20, 30, 255}, {40, 50, 60, 0}, {200, 100, 50, 255}};
static const char native_fragment[] = "<p>Wayland <i>rich</i> \xe6\x97\xa5\xe6\x9c\xac</p>";
static const char remote_document[] =
    "<html><body>\r\n<!--StartFragment--><b>RDP rich</b><!--EndFragment-->\r\n</body>\r\n</html>";
static const BYTE remote_rgb[6][3] = {{1, 2, 3}, {4, 5, 6}, {7, 8, 9}, {250, 251, 252}, {128, 64, 32}, {0, 0, 0}};
static BOOL rich(void) { return getenv("NIRI_RDP_TEST_CLIPBOARD_RICH") != NULL; }
static UINT32 le32(const BYTE *p) { return p[0] | (UINT32)p[1] << 8 | (UINT32)p[2] << 16 | (UINT32)p[3] << 24; }
static void put32(BYTE *p, UINT32 v) { for (int i = 0; i < 4; i++) p[i] = (BYTE)(v >> (8 * i)); }
/* The bridge converts the native PNG to a bottom-up 32-bit BI_RGB CF_DIB. */
static BOOL native_dib_ok(const BYTE *data, UINT32 length) {
    if (length != 40 + 24 || le32(data) != 40 || le32(data + 4) != 3 || le32(data + 8) != 2 ||
        data[12] != 1 || data[14] != 32 || le32(data + 16) != 0) return FALSE;
    for (int y = 0; y < 2; y++) for (int x = 0; x < 3; x++) {
        const BYTE *p = data + 40 + ((1 - y) * 3 + x) * 4, *e = native_rgba[y * 3 + x];
        if (p[0] != e[2] || p[1] != e[1] || p[2] != e[0] || p[3] != e[3]) return FALSE;
    }
    return TRUE;
}
static BOOL native_html_ok(const BYTE *data, UINT32 length) {
    char *text = calloc(1, (size_t)length + 1);
    if (!text) return FALSE;
    memcpy(text, data, length);
    const char *start = strstr(text, "StartFragment:"), *end = strstr(text, "EndFragment:");
    BOOL ok = FALSE;
    if (start && end) {
        unsigned long from = strtoul(start + 14, NULL, 10), to = strtoul(end + 12, NULL, 10);
        size_t expected = strlen(native_fragment);
        ok = from <= to && to <= length && to - from == expected && !memcmp(data + from, native_fragment, expected);
    }
    free(text);
    return ok;
}
static UINT advertise_rich(CliprdrClientContext *clip) {
    CLIPRDR_FORMAT formats[] = {{.formatId = CLIENT_HTML_ID, .formatName = (char *)"HTML Format"},
                                {.formatId = CF_DIB, .formatName = NULL}};
    CLIPRDR_FORMAT_LIST list = {.common = {.msgType = CB_FORMAT_LIST}, .numFormats = 2, .formats = formats};
    return clip->ClientFormatList(clip, &list);
}
static UINT send_rich(CliprdrClientContext *clip, UINT32 format) {
    Probe *probe = clip->custom;
    BYTE buffer[512];
    UINT32 length;
    if (format == CLIENT_HTML_ID) {
        const char *layout = "Version:0.9\r\nStartHTML:%010u\r\nEndHTML:%010u\r\nStartFragment:%010u\r\nEndFragment:%010u\r\n";
        unsigned base = (unsigned)snprintf(NULL, 0, layout, 0u, 0u, 0u, 0u);
        unsigned size = (unsigned)strlen(remote_document);
        unsigned from = (unsigned)(strstr(remote_document, "<b>") - remote_document);
        unsigned to = (unsigned)(strstr(remote_document, "<!--EndFragment") - remote_document);
        int header = snprintf((char *)buffer, sizeof(buffer), layout, base, base + size, base + from, base + to);
        if (header != (int)base || base + size + 1 > sizeof(buffer)) return CHANNEL_RC_BAD_CHANNEL;
        memcpy(buffer + base, remote_document, size + 1);
        length = base + size + 1;
    } else if (format == CF_DIB) {
        /* 24-bit bottom-up rows of nine bytes plus three bytes of padding. */
        memset(buffer, 0, 40 + 24);
        put32(buffer, 40); put32(buffer + 4, 3); put32(buffer + 8, 2);
        buffer[12] = 1; buffer[14] = 24; put32(buffer + 20, 24);
        for (int row = 0; row < 2; row++) for (int x = 0; x < 3; x++) {
            const BYTE *c = remote_rgb[(1 - row) * 3 + x];
            BYTE *p = buffer + 40 + row * 12 + x * 3;
            p[0] = c[2]; p[1] = c[1]; p[2] = c[0];
        }
        length = 40 + 24;
    } else {
        return CHANNEL_RC_BAD_CHANNEL;
    }
    CLIPRDR_FORMAT_DATA_RESPONSE response = {.common = {.msgType = CB_FORMAT_DATA_RESPONSE,
        .msgFlags = CB_RESPONSE_OK, .dataLen = length}, .requestedFormatData = buffer};
    UINT result = clip->ClientFormatDataResponse(clip, &response);
    if (result == CHANNEL_RC_OK && format == CF_DIB) { probe->sent = TRUE; puts("NIRI_RDP_CLIPBOARD_RDP_DATA_SENT"); }
    return result;
}
static UINT advertise(CliprdrClientContext *clip) {
    CLIPRDR_FORMAT format = {.formatId = 13, .formatName = NULL};
    CLIPRDR_FORMAT_LIST list = {.common = {.msgType = CB_FORMAT_LIST}, .numFormats = 1, .formats = &format};
    return clip->ClientFormatList(clip, &list);
}
static UINT ready(CliprdrClientContext *clip, const CLIPRDR_MONITOR_READY *ready) {
    (void)ready;
    CLIPRDR_GENERAL_CAPABILITY_SET general = {.capabilitySetType = CB_CAPSTYPE_GENERAL,
        .capabilitySetLength = CB_CAPSTYPE_GENERAL_LEN, .version = CB_CAPS_VERSION_2,
        .generalFlags = CB_USE_LONG_FORMAT_NAMES};
    CLIPRDR_CAPABILITIES caps = {.common = {.msgType = CB_CLIP_CAPS}, .cCapabilitiesSets = 1,
        .capabilitySets = (CLIPRDR_CAPABILITY_SET *)&general};
    UINT result = clip->ClientCapabilities(clip, &caps);
    if (result != CHANNEL_RC_OK) return result;
    CLIPRDR_FORMAT_LIST initial = {.common = {.msgType = CB_FORMAT_LIST}, .numFormats = 0};
    result = clip->ClientFormatList(clip, &initial);
    return result;
}
static UINT list_response(CliprdrClientContext *clip, const CLIPRDR_FORMAT_LIST_RESPONSE *response) {
    Probe *probe = clip->custom;
    if (!(response->common.msgFlags & CB_RESPONSE_OK)) return CHANNEL_RC_BAD_CHANNEL;
    if (!probe->received) puts("NIRI_RDP_CLIPBOARD_READY");
    return CHANNEL_RC_OK;
}
static UINT format_list(CliprdrClientContext *clip, const CLIPRDR_FORMAT_LIST *list) {
    CLIPRDR_FORMAT_LIST_RESPONSE ack = {.common = {.msgType = CB_FORMAT_LIST_RESPONSE, .msgFlags = CB_RESPONSE_OK}};
    UINT result = clip->ClientFormatListResponse(clip, &ack);
    if (result != CHANNEL_RC_OK) return result;
    Probe *probe = clip->custom;
    if (rich()) {
        UINT32 wanted = 0;
        BOOL png = FALSE;
        for (UINT32 i = 0; i < list->numFormats; i++) {
            const CLIPRDR_FORMAT *format = &list->formats[i];
            BOOL html = format->formatName && !strcmp(format->formatName, "HTML Format");
            png |= format->formatName && !strcmp(format->formatName, "PNG");
            if ((probe->rich_step == 0 && format->formatId == CF_DIB) || (probe->rich_step == 1 && html))
                wanted = format->formatId;
        }
        if (!wanted) return CHANNEL_RC_OK;
        if (probe->rich_step == 0 && !png) { probe->failed = TRUE; return CHANNEL_RC_BAD_CHANNEL; }
        probe->requested = wanted;
        CLIPRDR_FORMAT_DATA_REQUEST request = {.common = {.msgType = CB_FORMAT_DATA_REQUEST}, .requestedFormatId = wanted};
        return clip->ClientFormatDataRequest(clip, &request);
    }
    for (UINT32 i = 0; i < list->numFormats; i++) if (list->formats[i].formatId == 13) {
        CLIPRDR_FORMAT_DATA_REQUEST request = {.common = {.msgType = CB_FORMAT_DATA_REQUEST}, .requestedFormatId = 13};
        return clip->ClientFormatDataRequest(clip, &request);
    }
    return CHANNEL_RC_OK;
}
static UINT format_data(CliprdrClientContext *clip, const CLIPRDR_FORMAT_DATA_RESPONSE *response) {
    Probe *probe = clip->custom;
    if (rich()) {
        BOOL ok = (response->common.msgFlags & CB_RESPONSE_OK) != 0;
        if (ok && probe->rich_step == 0 && probe->requested == CF_DIB &&
            native_dib_ok(response->requestedFormatData, response->common.dataLen)) {
            probe->rich_step = 1;
            puts("NIRI_RDP_CLIPBOARD_IMAGE_OK");
            return CHANNEL_RC_OK;
        }
        if (ok && probe->rich_step == 1 && native_html_ok(response->requestedFormatData, response->common.dataLen)) {
            probe->rich_step = 2;
            probe->received = TRUE;
            puts("NIRI_RDP_CLIPBOARD_NATIVE_TO_RDP_OK");
            return advertise_rich(clip);
        }
        probe->failed = TRUE;
        return CHANNEL_RC_BAD_CHANNEL;
    }
    const void *expected = getenv("NIRI_RDP_TEST_CLIPBOARD_EMPTY") ? empty_text : native_text;
    size_t length = getenv("NIRI_RDP_TEST_CLIPBOARD_EMPTY") ? sizeof(empty_text) : sizeof(native_text);
    if (!(response->common.msgFlags & CB_RESPONSE_OK) || response->common.dataLen != length ||
        memcmp(response->requestedFormatData, expected, length)) {
        if ((response->common.msgFlags & CB_RESPONSE_OK) &&
            response->common.dataLen == sizeof(intermediate_text) &&
            !memcmp(response->requestedFormatData, intermediate_text, sizeof(intermediate_text)))
            return CHANNEL_RC_OK; /* A valid older ownership snapshot may finish. */
        probe->failed = TRUE;
        return CHANNEL_RC_BAD_CHANNEL;
    }
    probe->received = TRUE;
    puts("NIRI_RDP_CLIPBOARD_NATIVE_TO_RDP_OK");
    return advertise(clip);
}
static UINT data_request(CliprdrClientContext *clip, const CLIPRDR_FORMAT_DATA_REQUEST *request) {
    Probe *probe = clip->custom;
    if (rich()) return probe->rich_step == 2 ? send_rich(clip, request->requestedFormatId) : CHANNEL_RC_BAD_CHANNEL;
    if (request->requestedFormatId != 13) return CHANNEL_RC_BAD_CHANNEL;
    const void *data = getenv("NIRI_RDP_TEST_CLIPBOARD_EMPTY") ? empty_text : remote_text;
    size_t length = getenv("NIRI_RDP_TEST_CLIPBOARD_EMPTY") ? sizeof(empty_text) : sizeof(remote_text);
    CLIPRDR_FORMAT_DATA_RESPONSE response = {.common = {.msgType = CB_FORMAT_DATA_RESPONSE,
        .msgFlags = CB_RESPONSE_OK, .dataLen = (UINT32)length}, .requestedFormatData = data};
    UINT result = clip->ClientFormatDataResponse(clip, &response);
    if (result == CHANNEL_RC_OK) { probe->sent = TRUE; puts("NIRI_RDP_CLIPBOARD_RDP_DATA_SENT"); }
    return result;
}
static void channel_connected(void *context, const ChannelConnectedEventArgs *event) {
    if (strcmp(event->name, CLIPRDR_SVC_CHANNEL_NAME)) return;
    Probe *probe = context;
    probe->clip = event->pInterface;
    probe->clip->custom = probe;
    probe->clip->MonitorReady = ready;
    probe->clip->ServerFormatList = format_list;
    probe->clip->ServerFormatListResponse = list_response;
    probe->clip->ServerFormatDataRequest = data_request;
    probe->clip->ServerFormatDataResponse = format_data;
}
static BOOL pre_connect(freerdp *rdp) {
    return PubSub_SubscribeChannelConnected(rdp->context->pubSub, channel_connected) >= 0;
}
static BOOL load_channels(freerdp *rdp) { return freerdp_client_load_addins(rdp->context->channels, rdp->context->settings); }
static BOOL post_connect(freerdp *rdp) {
    ((Probe *)rdp->context)->initialized = gdi_init(rdp, PIXEL_FORMAT_BGRA32);
    return ((Probe *)rdp->context)->initialized;
}
static BOOL no_prompt(freerdp *rdp, char **user, char **pass, char **domain, rdp_auth_reason reason) {
    (void)rdp; (void)user; (void)pass; (void)domain; (void)reason; return FALSE;
}
int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    const char *user = getenv("NIRI_RDP_TEST_USERNAME"), *pass = getenv("NIRI_RDP_TEST_PASSWORD");
    if (!user || !pass) return 2;
    if (freerdp_register_addin_provider(freerdp_channels_load_static_addin_entry, 0)) return 2;
    freerdp *rdp = freerdp_new(); if (!rdp) return 2;
    rdp->ContextSize = sizeof(Probe); rdp->PreConnect = pre_connect; rdp->PostConnect = post_connect;
    rdp->LoadChannels = load_channels; rdp->AuthenticateEx = no_prompt;
    if (!freerdp_context_new(rdp)) { freerdp_free(rdp); return 2; }
    rdpSettings *s = rdp->context->settings;
    BOOL nla = getenv("NIRI_RDP_TEST_NLA") != NULL;
    BOOL configured = freerdp_settings_set_string(s, FreeRDP_ServerHostname, "127.0.0.1") &&
        freerdp_settings_set_uint32(s, FreeRDP_ServerPort, 33991) &&
        freerdp_settings_set_string(s, FreeRDP_Username, user) && freerdp_settings_set_string(s, FreeRDP_Password, pass) &&
        freerdp_settings_set_bool(s, FreeRDP_NlaSecurity, nla) && freerdp_settings_set_bool(s, FreeRDP_TlsSecurity, !nla) &&
        freerdp_settings_set_bool(s, FreeRDP_RdpSecurity, FALSE) && freerdp_settings_set_bool(s, FreeRDP_IgnoreCertificate, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_AutoLogonEnabled, TRUE) && freerdp_settings_set_bool(s, FreeRDP_RedirectClipboard, TRUE) &&
        freerdp_settings_set_uint32(s, FreeRDP_DesktopWidth, 1001) && freerdp_settings_set_uint32(s, FreeRDP_DesktopHeight, 701);
    int result = 2;
    if (configured && freerdp_connect(rdp)) {
        Probe *probe = (Probe *)rdp->context;
        ULONGLONG deadline = GetTickCount64() + 15000;
        while (!probe->failed && GetTickCount64() < deadline) {
            struct pollfd input = {.fd = STDIN_FILENO, .events = POLLIN};
            if (probe->received && probe->sent && poll(&input, 1, 0) > 0) {
                char command[32];
                if (fgets(command, sizeof(command), stdin) && !strcmp(command, "stop\n")) { result = 0; break; }
            }
            HANDLE events[64]; DWORD n = freerdp_get_event_handles(rdp->context, events, 64);
            if (!n || WaitForMultipleObjects(n, events, FALSE, 50) == WAIT_FAILED || !freerdp_check_event_handles(rdp->context)) break;
        }
        if (!freerdp_disconnect(rdp)) result = 2;
    }
    if (((Probe *)rdp->context)->initialized) gdi_free(rdp);
    freerdp_context_free(rdp); freerdp_free(rdp);
    return result;
}
