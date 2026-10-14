#define _POSIX_C_SOURCE 200809L
/* A real CLIPRDR client restricted to known disposable test strings. */
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
} Probe;
static const UINT16 native_text[] = {'W','a','y','l','a','n','d',' ',0x65e5,0x672c,0xd83d,0xde00,'\r','\n','l','i','n','e',' ', '2',0};
static const UINT16 remote_text[] = {'R','D','P',' ',0x65e5,0x672c,0xd83d,0xde00,'\r','\n','l','i','n','e',' ', '2',0};
static const UINT16 empty_text[] = {0};
static const UINT16 intermediate_text[] = {'s','t','a','l','e',0};
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
    for (UINT32 i = 0; i < list->numFormats; i++) if (list->formats[i].formatId == 13) {
        CLIPRDR_FORMAT_DATA_REQUEST request = {.common = {.msgType = CB_FORMAT_DATA_REQUEST}, .requestedFormatId = 13};
        return clip->ClientFormatDataRequest(clip, &request);
    }
    return CHANNEL_RC_OK;
}
static UINT format_data(CliprdrClientContext *clip, const CLIPRDR_FORMAT_DATA_RESPONSE *response) {
    Probe *probe = clip->custom;
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
