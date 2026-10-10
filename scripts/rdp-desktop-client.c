#define _POSIX_C_SOURCE 200809L
/* Displayless real-desktop probe. FreeRDP owns all RDP protocol/security. */
#include <stdio.h>
#include <stdlib.h>
#include <freerdp/freerdp.h>
#include <freerdp/settings.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/input.h>
#include <freerdp/error.h>
#include <winpr/synch.h>
#include <winpr/sysinfo.h>

typedef struct { rdpContext context; unsigned phase; BOOL initialized; } Probe;

static BOOL begin_paint(rdpContext *ctx) {
    ctx->gdi->primary->hdc->hwnd->invalid->null = TRUE;
    ctx->gdi->primary->hdc->hwnd->ninvalid = 0;
    return TRUE;
}

static BOOL contains_color(rdpGdi *gdi, BYTE b, BYTE g, BYTE r) {
    if (!gdi || !gdi->primary_buffer || gdi->width < 1 || gdi->height < 1) return FALSE;
    /* Require a substantial app region, rather than an incidental cursor pixel. */
    unsigned count = 0;
    for (UINT32 y = 0; y < (UINT32)gdi->height; y += 4)
        for (UINT32 x = 0; x < (UINT32)gdi->width; x += 4) {
            const BYTE *p = gdi->primary_buffer + y * gdi->stride + x * 4;
            if (p[0] == b && p[1] == g && p[2] == r && ++count >= 64) return TRUE;
        }
    return FALSE;
}

static BOOL end_paint(rdpContext *ctx) {
    Probe *probe = (Probe *)ctx;
    if (probe->phase == 0 && contains_color(ctx->gdi, 180, 40, 30)) {
        if (!freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_DOWN, 30)) return FALSE;
        probe->phase = 1;
    } else if (probe->phase == 1 && contains_color(ctx->gdi, 30, 40, 180)) {
        if ((!getenv("NIRI_RDP_TEST_HOLD") && !getenv("NIRI_RDP_TEST_CRASH") &&
             !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_RELEASE, 30)) ||
            !freerdp_input_send_mouse_event(ctx->input, PTR_FLAGS_MOVE, 320, 240) ||
            !freerdp_input_send_mouse_event(ctx->input, PTR_FLAGS_BUTTON1 | PTR_FLAGS_DOWN, 320, 240)) return FALSE;
        probe->phase = 2;
    } else if (probe->phase == 2 && contains_color(ctx->gdi, 40, 180, 30)) {
        if (getenv("NIRI_RDP_TEST_CRASH")) return TRUE;
        if (getenv("NIRI_RDP_TEST_HOLD")) { probe->phase = 4; return TRUE; }
        if (!freerdp_input_send_mouse_event(ctx->input, PTR_FLAGS_BUTTON1, 320, 240)) return FALSE;
        probe->phase = 3;
    } else if (probe->phase == 3 && contains_color(ctx->gdi, 180, 40, 30)) {
        probe->phase = 4;
    }
    return TRUE;
}

static BOOL post_connect(freerdp *rdp) {
    const BOOL nla = getenv("NIRI_RDP_TEST_NLA") != NULL;
    UINT32 protocol = freerdp_settings_get_uint32(rdp->context->settings, FreeRDP_SelectedProtocol);
    if ((nla && protocol != 2 && protocol != 8) || (!nla && protocol != 1)) return FALSE;
    if (!gdi_init(rdp, PIXEL_FORMAT_BGRA32)) return FALSE;
    ((Probe *)rdp->context)->initialized = TRUE;
    rdp->context->update->BeginPaint = begin_paint;
    rdp->context->update->EndPaint = end_paint;
    return TRUE;
}

static BOOL no_prompt(freerdp *rdp, char **user, char **password, char **domain, rdp_auth_reason reason) {
    (void)rdp; (void)user; (void)password; (void)domain; (void)reason;
    return FALSE;
}

int main(void) {
    const char *user = getenv("NIRI_RDP_TEST_USERNAME");
    const char *pass = getenv("NIRI_RDP_TEST_PASSWORD");
    if (!user || !*user || !pass || !*pass) return 2;
    BOOL nla = getenv("NIRI_RDP_TEST_NLA") != NULL;
    freerdp *rdp = freerdp_new();
    if (!rdp) return 2;
    rdp->ContextSize = sizeof(Probe);
    rdp->PostConnect = post_connect;
    rdp->AuthenticateEx = no_prompt;
    if (!freerdp_context_new(rdp)) { freerdp_free(rdp); return 2; }
    rdpSettings *s = rdp->context->settings;
    BOOL configured = freerdp_settings_set_string(s, FreeRDP_ServerHostname, "127.0.0.1") &&
        freerdp_settings_set_uint32(s, FreeRDP_ServerPort, 33991) &&
        freerdp_settings_set_string(s, FreeRDP_Username, user) &&
        freerdp_settings_set_string(s, FreeRDP_Password, pass) &&
        freerdp_settings_set_string(s, FreeRDP_Domain, "") &&
        freerdp_settings_set_bool(s, FreeRDP_NlaSecurity, nla) &&
        freerdp_settings_set_bool(s, FreeRDP_TlsSecurity, !nla) &&
        freerdp_settings_set_bool(s, FreeRDP_RdpSecurity, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_AutoLogonEnabled, TRUE) &&
        /* Only this disposable loopback probe accepts its ephemeral certificate. */
        freerdp_settings_set_bool(s, FreeRDP_IgnoreCertificate, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_SoftwareGdi, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_SupportGraphicsPipeline, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_CompressionEnabled, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_BitmapCacheEnabled, FALSE) &&
        freerdp_settings_set_uint32(s, FreeRDP_DesktopWidth, 640) &&
        freerdp_settings_set_uint32(s, FreeRDP_DesktopHeight, 480) &&
        freerdp_settings_set_uint32(s, FreeRDP_ColorDepth, 32) &&
        freerdp_settings_set_uint32(s, FreeRDP_TcpConnectTimeout, 5000);
    int result = 2;
    if (configured && freerdp_connect(rdp)) {
        Probe *probe = (Probe *)rdp->context;
        ULONGLONG deadline = GetTickCount64() + 15000;
        while (probe->phase < 4 && GetTickCount64() < deadline) {
            HANDLE events[64];
            DWORD n = freerdp_get_event_handles(rdp->context, events, 64);
            if (!n || WaitForMultipleObjects(n, events, FALSE, 100) == WAIT_FAILED ||
                !freerdp_check_event_handles(rdp->context)) break;
        }
        if (probe->phase == 4) { puts("NIRI_RDP_APP_KEY_POINTER_RELEASE_OK"); result = 0; }
        else fprintf(stderr, "app input/frame acknowledgement failed in phase %u\n", probe->phase);
        if (!freerdp_disconnect(rdp)) result = 2;
    } else if (freerdp_get_last_error(rdp->context) == FREERDP_ERROR_AUTHENTICATION_FAILED ||
               freerdp_get_last_error(rdp->context) == FREERDP_ERROR_CONNECT_LOGON_FAILURE) {
        puts("RDP_AUTHENTICATION_REJECTED"); result = 4;
    } else fprintf(stderr, "connection failed: %s\n", freerdp_get_last_error_name(freerdp_get_last_error(rdp->context)));
    if (((Probe *)rdp->context)->initialized) gdi_free(rdp);
    freerdp_context_free(rdp); freerdp_free(rdp);
    return result;
}
