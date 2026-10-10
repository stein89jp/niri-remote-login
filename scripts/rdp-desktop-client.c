#define _POSIX_C_SOURCE 200809L
/* Displayless real-desktop probe. FreeRDP owns all RDP protocol/security. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <freerdp/freerdp.h>
#include <freerdp/client/channels.h>
#include <freerdp/client/cmdline.h>
#include <freerdp/gdi/gfx.h>
#include <freerdp/settings.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/input.h>
#include <freerdp/error.h>
#include <winpr/synch.h>
#include <winpr/sysinfo.h>

typedef struct {
    rdpContext context;
    unsigned phase, avc_frames;
    BOOL initialized, gfx_requested, gfx_failed;
    pcRdpgfxSurfaceCommand surface_command;
} Probe;

static UINT decode_surface(RdpgfxClientContext *gfx, const RDPGFX_SURFACE_COMMAND *command) {
    rdpGdi *gdi = (rdpGdi *)gfx->custom;
    Probe *probe = (Probe *)gdi->context;
    UINT result = probe->surface_command(gfx, command);
    if (result == CHANNEL_RC_OK && command->codecId == RDPGFX_CODECID_AVC420)
        probe->avc_frames++;
    return result;
}

static void channel_connected(void *context, const ChannelConnectedEventArgs *event) {
    Probe *probe = context;
    if (strcmp(event->name, RDPGFX_DVC_CHANNEL_NAME) != 0) return;
    RdpgfxClientContext *gfx = event->pInterface;
    if (!gdi_graphics_pipeline_init(probe->context.gdi, gfx)) {
        probe->gfx_failed = TRUE;
        return;
    }
    probe->surface_command = gfx->SurfaceCommand;
    gfx->SurfaceCommand = decode_surface;
}

static void channel_disconnected(void *context, const ChannelDisconnectedEventArgs *event) {
    Probe *probe = context;
    if (strcmp(event->name, RDPGFX_DVC_CHANNEL_NAME) == 0 && probe->initialized)
        gdi_graphics_pipeline_uninit(probe->context.gdi, event->pInterface);
}

static BOOL pre_connect(freerdp *rdp) {
    if (!((Probe *)rdp->context)->gfx_requested) return TRUE;
    return PubSub_SubscribeChannelConnected(rdp->context->pubSub, channel_connected) >= 0 &&
        PubSub_SubscribeChannelDisconnected(rdp->context->pubSub, channel_disconnected) >= 0;
}

static BOOL load_channels(freerdp *rdp) {
    return freerdp_client_load_addins(rdp->context->channels, rdp->context->settings);
}

static BOOL begin_paint(rdpContext *ctx) {
    ctx->gdi->primary->hdc->hwnd->invalid->null = TRUE;
    ctx->gdi->primary->hdc->hwnd->ninvalid = 0;
    return TRUE;
}

static BOOL contains_color(rdpGdi *gdi, BYTE b, BYTE g, BYTE r) {
    if (!gdi || !gdi->primary_buffer || gdi->width < 1 || gdi->height < 1) return FALSE;
    /* Require a substantial app region, rather than an incidental cursor pixel. */
    unsigned count = 0;
    int tolerance = ((Probe *)gdi->context)->gfx_requested ? 15 : 0;
    for (UINT32 y = 0; y < (UINT32)gdi->height; y += 4)
        for (UINT32 x = 0; x < (UINT32)gdi->width; x += 4) {
            const BYTE *p = gdi->primary_buffer + y * gdi->stride + x * 4;
            if (abs((int)p[0] - b) <= tolerance && abs((int)p[1] - g) <= tolerance &&
                abs((int)p[2] - r) <= tolerance && ++count >= 64) return TRUE;
        }
    return FALSE;
}

static BOOL end_paint(rdpContext *ctx) {
    Probe *probe = (Probe *)ctx;
    if (probe->gfx_failed) return FALSE;
    if (probe->gfx_requested && probe->avc_frames == 0) return TRUE;
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

static BOOL desktop_resize(rdpContext *ctx) {
    return gdi_resize(ctx->gdi,
                      freerdp_settings_get_uint32(ctx->settings, FreeRDP_DesktopWidth),
                      freerdp_settings_get_uint32(ctx->settings, FreeRDP_DesktopHeight));
}

static BOOL post_connect(freerdp *rdp) {
    const BOOL nla = getenv("NIRI_RDP_TEST_NLA") != NULL;
    UINT32 protocol = freerdp_settings_get_uint32(rdp->context->settings, FreeRDP_SelectedProtocol);
    if ((nla && protocol != 2 && protocol != 8) || (!nla && protocol != 1)) return FALSE;
    if (!gdi_init(rdp, PIXEL_FORMAT_BGRA32)) return FALSE;
    ((Probe *)rdp->context)->initialized = TRUE;
    rdp->context->update->BeginPaint = begin_paint;
    rdp->context->update->EndPaint = end_paint;
    rdp->context->update->DesktopResize = desktop_resize;
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
    BOOL gfx = getenv("NIRI_RDP_TEST_GFX") != NULL;
    if (gfx && freerdp_register_addin_provider(freerdp_channels_load_static_addin_entry, 0) != 0)
        return 2;
    freerdp *rdp = freerdp_new();
    if (!rdp) return 2;
    rdp->ContextSize = sizeof(Probe);
    rdp->PostConnect = post_connect;
    rdp->PreConnect = pre_connect;
    if (gfx) rdp->LoadChannels = load_channels;
    rdp->AuthenticateEx = no_prompt;
    if (!freerdp_context_new(rdp)) { freerdp_free(rdp); return 2; }
    ((Probe *)rdp->context)->gfx_requested = gfx;
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
        freerdp_settings_set_bool(s, FreeRDP_SupportGraphicsPipeline, gfx) &&
        freerdp_settings_set_bool(s, FreeRDP_SupportDynamicChannels, gfx) &&
        freerdp_settings_set_bool(s, FreeRDP_GfxH264, gfx) &&
        freerdp_settings_set_bool(s, FreeRDP_GfxAVC444, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_GfxAVC444v2, FALSE) &&
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
        if (probe->phase == 4 && (!gfx || probe->avc_frames > 0)) {
            puts("NIRI_RDP_APP_KEY_POINTER_RELEASE_OK");
            if (gfx) printf("NIRI_RDP_AVC420_DECODED_FRAMES %u\n", probe->avc_frames);
            result = 0;
        }
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
