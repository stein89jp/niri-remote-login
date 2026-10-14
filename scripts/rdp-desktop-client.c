#define _POSIX_C_SOURCE 200809L
/* Displayless real-desktop probe. FreeRDP owns all RDP protocol/security. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <poll.h>
#include <unistd.h>
#include <inttypes.h>
#include <freerdp/freerdp.h>
#include <freerdp/client/channels.h>
#include <freerdp/client/cmdline.h>
#include <freerdp/client/disp.h>
#include <freerdp/gdi/gfx.h>
#include <freerdp/settings.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/input.h>
#include <freerdp/error.h>
#include <winpr/synch.h>
#include <winpr/sysinfo.h>

typedef struct {
    rdpContext context;
    unsigned phase, avc_frames, avc444_frames, layout_index, login_phase;
    unsigned performance_seconds, decoded_updates, interval_count, interval_cursor;
    unsigned refresh_stage, refresh_previous;
    ULONGLONG refresh_at;
    ULONGLONG performance_start, previous_update;
    ULONGLONG intervals[4096];
    ULONGLONG login_retry_at;
    UINT32 expected_width, expected_height;
    UINT16 pointer_x, pointer_y;
    BOOL initialized, gfx_requested, gfx_failed, disp_ready, size_verified, awaiting_ack;
    DispClientContext *disp;
    RdpgfxClientContext *gfx;
} Probe;

/* Negotiation allows odd widths; DISP widths are even. Exercise codec padding. */
static const UINT32 layouts[][2] = {{1001, 701}, {702, 1001}, {1002, 701}, {702, 1001}};

static UINT display_caps(DispClientContext *disp, UINT32 monitors, UINT32 a, UINT32 b) {
    Probe *probe = disp->custom;
    (void)a; (void)b;
    if (monitors < 1) return CHANNEL_RC_BAD_CHANNEL;
    probe->disp_ready = TRUE;
    return CHANNEL_RC_OK;
}

static void update_codec_stats(Probe *probe) {
    if (!probe->gfx) return;
    /* FreeRDP records received codec commands after dispatch. Its GDI pipeline
     * may replace SurfaceCommand during reset, so do not wrap that callback.
     * Successful colored-pixel checks below independently prove decoding. */
    probe->avc_frames = (unsigned)rdpgfx_stats_value_for_index(probe->gfx, RDPGFX_CODECID_AVC420);
    probe->avc444_frames = (unsigned)(rdpgfx_stats_value_for_index(probe->gfx, RDPGFX_CODECID_AVC444) +
                                    rdpgfx_stats_value_for_index(probe->gfx, RDPGFX_CODECID_AVC444v2));
}

static void channel_connected(void *context, const ChannelConnectedEventArgs *event) {
    Probe *probe = context;
    if (strcmp(event->name, DISP_DVC_CHANNEL_NAME) == 0) {
        probe->disp = event->pInterface;
        probe->disp->custom = probe;
        probe->disp->DisplayControlCaps = display_caps;
        return;
    }
    if (strcmp(event->name, RDPGFX_DVC_CHANNEL_NAME) != 0) return;
    RdpgfxClientContext *gfx = event->pInterface;
    if (!gdi_graphics_pipeline_init(probe->context.gdi, gfx)) {
        probe->gfx_failed = TRUE;
        return;
    }
    probe->gfx = gfx;
}

static void channel_disconnected(void *context, const ChannelDisconnectedEventArgs *event) {
    Probe *probe = context;
    if (strcmp(event->name, DISP_DVC_CHANNEL_NAME) == 0) {
        probe->disp = NULL;
        probe->disp_ready = FALSE;
    }
    if (strcmp(event->name, RDPGFX_DVC_CHANNEL_NAME) == 0 && probe->initialized) {
        update_codec_stats(probe);
        probe->gfx = NULL;
        gdi_graphics_pipeline_uninit(probe->context.gdi, event->pInterface);
    }
}

static BOOL pre_connect(freerdp *rdp) {
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

static BOOL login_text(rdpContext *ctx, const char *text) {
    /* Exercise ordinary mstsc scancode input. Fixture credentials are ASCII. */
    static const struct { UINT16 first; const char *normal, *shifted; } rows[] = {
        {2, "1234567890-^", "!\"#$%&'()~=~"}, {16, "qwertyuiop@[", "QWERTYUIOP`{"},
        {30, "asdfghjkl;:", "ASDFGHJKL+*"}, {43, "]", "}"},
        {44, "zxcvbnm,./", "ZXCVBNM<>?"}, {57, " ", " "}
    };
    for (const unsigned char *p = (const unsigned char *)text; *p; p++) {
        UINT16 code = 0;
        BOOL shift = FALSE;
        for (unsigned i = 0; i < sizeof(rows) / sizeof(rows[0]); i++) {
            const char *found = strchr(rows[i].normal, *p);
            if (!found) { found = strchr(rows[i].shifted, *p); shift = found != NULL; }
            if (found) { code = rows[i].first + (UINT16)(found - (shift ? rows[i].shifted : rows[i].normal)); break; }
        }
        if (!code || (shift && !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_DOWN, 42)) ||
            !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_DOWN, code) ||
            !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_RELEASE, code) ||
            (shift && !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_RELEASE, 42))) return FALSE;
    }
    return TRUE;
}

static BOOL unicode_unit(rdpContext *ctx, UINT16 unit) {
    return freerdp_input_send_unicode_keyboard_event(ctx->input, 0, unit) &&
        freerdp_input_send_unicode_keyboard_event(ctx->input, KBD_FLAGS_RELEASE, unit);
}

static BOOL login_key(rdpContext *ctx, UINT16 code) {
    return freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_DOWN, code) &&
        freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_RELEASE, code);
}

static BOOL end_paint(rdpContext *ctx) {
    Probe *probe = (Probe *)ctx;
    if (probe->gfx_failed) return FALSE;
    if (probe->performance_seconds) {
        ULONGLONG now = GetTickCount64();
        if (probe->previous_update) {
            probe->intervals[probe->interval_cursor] = now - probe->previous_update;
            probe->interval_cursor = (probe->interval_cursor + 1) % 4096;
            if (probe->interval_count < 4096) probe->interval_count++;
        }
        probe->previous_update = now;
        probe->decoded_updates++;
        probe->phase = 4;
        return TRUE;
    }
    if (getenv("NIRI_RDP_TEST_LOGIN_SCREEN")) {
        if (probe->login_phase == 0 && contains_color(ctx->gdi, 140, 95, 40)) {
            puts("NIRI_RDP_LOGIN_SCREEN_WITHOUT_CLIENT_CREDENTIALS");
            if (!login_text(ctx, getenv("NIRI_RDP_TEST_USERNAME")) || !login_key(ctx, 15) ||
                !login_text(ctx, "incorrect-test-password") || !login_key(ctx, 28)) return FALSE;
            probe->login_phase = 1;
            return TRUE;
        }
        if (probe->login_phase == 1 && contains_color(ctx->gdi, 80, 80, 140)) {
            puts("NIRI_RDP_LOGIN_SCREEN_WRONG_PASSWORD_REJECTED");
            probe->login_phase = 2;
            probe->login_retry_at = GetTickCount64() + 2200;
            return TRUE;
        }
        if (probe->login_phase < 3) return TRUE;
    }
    /* Initial bitmaps can arrive while GFX is still negotiating. Wait for an
     * actual AVC command before driving input/resize, otherwise a fast bitmap
     * roundtrip deactivates the connection before its first encoder completes.
     * The event loop revisits the decoded image after dispatch records stats. */
    if (probe->gfx_requested && !probe->avc_frames && !probe->avc444_frames) return TRUE;
    if (ctx->gdi->width != (INT32)probe->expected_width ||
        ctx->gdi->height != (INT32)probe->expected_height) return TRUE;
    if (!probe->size_verified && contains_color(ctx->gdi, 180, 40, 30)) {
        probe->size_verified = TRUE;
        printf("NIRI_RDP_DESKTOP_SIZE %u %u\n", probe->expected_width, probe->expected_height);
        probe->awaiting_ack = getenv("NIRI_RDP_TEST_RESIZE_ACK") != NULL;
    }
    if (!probe->size_verified || probe->awaiting_ack) return TRUE;
    if (probe->phase == 0 && contains_color(ctx->gdi, 180, 40, 30)) {
        if (!freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_DOWN, 30)) return FALSE;
        probe->phase = 1;
    } else if (probe->phase == 1 && contains_color(ctx->gdi, 30, 40, 180)) {
        if ((!getenv("NIRI_RDP_TEST_HOLD") && !getenv("NIRI_RDP_TEST_CRASH") &&
             !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_RELEASE, 30)) ||
            !freerdp_input_send_mouse_event(ctx->input, PTR_FLAGS_MOVE, probe->pointer_x, probe->pointer_y) ||
            !freerdp_input_send_mouse_event(ctx->input, PTR_FLAGS_BUTTON1 | PTR_FLAGS_DOWN,
                                           probe->pointer_x, probe->pointer_y)) return FALSE;
        probe->phase = 2;
    } else if (probe->phase == 2 && contains_color(ctx->gdi, 40, 180, 30)) {
        if (getenv("NIRI_RDP_TEST_CRASH")) return TRUE;
        if (getenv("NIRI_RDP_TEST_HOLD")) { probe->phase = 4; return TRUE; }
        if (!freerdp_input_send_mouse_event(ctx->input, PTR_FLAGS_BUTTON1,
                                           probe->pointer_x, probe->pointer_y)) return FALSE;
        probe->phase = 3;
    } else if (probe->phase == 3 && contains_color(ctx->gdi, 180, 40, 30)) {
        if (getenv("NIRI_RDP_TEST_IME")) {
            if (!login_key(ctx, 1) ||
                !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_DOWN, 29) ||
                !login_key(ctx, 57) ||
                !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_RELEASE, 29) ||
                !login_text(ctx, "nihon") || !login_key(ctx, 57) || !login_key(ctx, 28) ||
                !login_text(ctx, "nihon") || !login_key(ctx, 65) || !login_key(ctx, 28)) return FALSE;
            static const UINT16 committed[] = {'U', '@', 0x65e5, 0x672c, 0xd83d, 0xde00};
            for (unsigned i = 0; i < sizeof(committed) / sizeof(committed[0]); i++)
                if (!unicode_unit(ctx, committed[i])) return FALSE;
            if (!unicode_unit(ctx, '\r')) return FALSE;
            puts("NIRI_RDP_IME_SEQUENCE_SENT");
        }
        // Consecutive characters, modifier release, Enter and a compositor bind.
        // The Wayland fixture decodes this with the advertised XKB keymap.
        if (getenv("NIRI_RDP_TEST_KEYBOARD") && !getenv("NIRI_RDP_TEST_IME")) {
            if (!login_key(ctx, 1) || !login_text(ctx, "abc123ABc") || !login_key(ctx, 28) ||
                !login_text(ctx, "@\"^&:[]") || !login_key(ctx, 0x73) ||
                !login_key(ctx, 0x7d) || !login_key(ctx, 28) ||
                !login_key(ctx, 0x70) || !login_key(ctx, 0x79) || !login_key(ctx, 0x7b)) return FALSE;
            /* Mobile-style UTF-16, including releases between surrogate halves.
             * Held Shift must survive text injection and physical-key resumption. */
            static const UINT16 phone[] = {'P', 'h', 'o', 'n', 'e', '@', '"', 0x65e5, 0x672c, 0xd83d, 0xde00};
            if (!freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_DOWN, 42)) return FALSE;
            for (unsigned i = 0; i < sizeof(phone) / sizeof(phone[0]); i++)
                if (!unicode_unit(ctx, phone[i])) return FALSE;
            if (!unicode_unit(ctx, 'x') || !unicode_unit(ctx, 'x') ||
                !unicode_unit(ctx, 8) || !unicode_unit(ctx, 8) || !login_key(ctx, 30) ||
                !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_RELEASE, 42) ||
                !login_text(ctx, "a@") || !unicode_unit(ctx, '\r') ||
                !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_EXTENDED | KBD_FLAGS_DOWN, 0x5b) ||
                !login_key(ctx, 32) ||
                !freerdp_input_send_keyboard_event(ctx->input, KBD_FLAGS_EXTENDED | KBD_FLAGS_RELEASE, 0x5b))
                return FALSE;
            puts("NIRI_RDP_KEYBOARD_SEQUENCE_SENT");
        }
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
    ((Probe *)rdp->context)->performance_start = GetTickCount64();
    rdp->context->update->BeginPaint = begin_paint;
    rdp->context->update->EndPaint = end_paint;
    rdp->context->update->DesktopResize = desktop_resize;
    return TRUE;
}

static BOOL no_prompt(freerdp *rdp, char **user, char **password, char **domain, rdp_auth_reason reason) {
    (void)rdp; (void)user; (void)password; (void)domain; (void)reason;
    return FALSE;
}

static BOOL refresh_step(Probe *probe) {
    rdpContext *ctx = &probe->context;
    ULONGLONG now = GetTickCount64(), elapsed = now - probe->performance_start;
    RECTANGLE_16 area = { .left = 0, .top = 0,
        .right = (UINT16)(ctx->gdi->width - 1), .bottom = (UINT16)(ctx->gdi->height - 1) };
    if (!getenv("NIRI_RDP_TEST_REFRESH")) return TRUE;
    switch (probe->refresh_stage) {
    case 0:
        if (elapsed < 1500 || !probe->decoded_updates) return TRUE;
        probe->refresh_previous = probe->decoded_updates;
        probe->refresh_at = now;
        if (!ctx->update->RefreshRect || !ctx->update->RefreshRect(ctx, 1, &area)) return FALSE;
        probe->refresh_stage = 1;
        puts("NIRI_RDP_REFRESH_SENT");
        break;
    case 1:
        if (probe->decoded_updates > probe->refresh_previous) {
            puts("NIRI_RDP_REFRESH_DECODED"); probe->refresh_stage = 2;
        } else if (now - probe->refresh_at > 3000) return FALSE;
        break;
    case 2:
        if (elapsed < 2500) return TRUE;
        probe->refresh_previous = probe->decoded_updates;
        probe->refresh_at = now;
        if (!ctx->update->SuppressOutput || !ctx->update->SuppressOutput(ctx, 0, NULL)) return FALSE;
        probe->refresh_stage = 3;
        puts("NIRI_RDP_SUPPRESS_SENT");
        break;
    case 3:
        if (now - probe->refresh_at < 1000) return TRUE;
        if (probe->decoded_updates > probe->refresh_previous + 1) return FALSE;
        probe->refresh_previous = probe->decoded_updates;
        probe->refresh_at = now;
        if (!ctx->update->SuppressOutput(ctx, 1, &area)) return FALSE;
        probe->refresh_stage = 4;
        puts("NIRI_RDP_RESUME_SENT");
        break;
    case 4:
        if (probe->decoded_updates > probe->refresh_previous) {
            puts("NIRI_RDP_RESUME_DECODED"); probe->refresh_stage = 5;
        } else if (now - probe->refresh_at > 3000) return FALSE;
        break;
    default: break;
    }
    return TRUE;
}

static int compare_interval(const void *a, const void *b) {
    ULONGLONG first = *(const ULONGLONG *)a, second = *(const ULONGLONG *)b;
    return (first > second) - (first < second);
}
static BOOL performance_receipt(Probe *probe) {
    ULONGLONG elapsed = GetTickCount64() - probe->performance_start;
    qsort(probe->intervals, probe->interval_count, sizeof(probe->intervals[0]), compare_interval);
    unsigned count = probe->interval_count;
    printf("NIRI_RDP_PERFORMANCE {\"elapsed_ms\":%" PRIu64 ",\"decoded_updates\":%u,\"decoded_avc420_frames\":%u,\"decoded_avc444_streams\":%u,\"decoded_update_rate\":%.3f,\"update_interval_p50_us\":%" PRIu64 ",\"update_interval_p95_us\":%" PRIu64 ",\"update_interval_max_us\":%" PRIu64 ",\"physical_presentation_measured\":false}\n",
           (uint64_t)elapsed, probe->decoded_updates, probe->avc_frames, probe->avc444_frames,
           elapsed ? 1000.0 * probe->decoded_updates / elapsed : 0,
           count ? (uint64_t)probe->intervals[(count - 1) / 2] * 1000 : 0,
           count ? (uint64_t)probe->intervals[(count - 1) * 95 / 100] * 1000 : 0,
           count ? (uint64_t)probe->intervals[count - 1] * 1000 : 0);
    const char *path = getenv("NIRI_RDP_TEST_SCREENSHOT");
    if (path) {
        FILE *image = fopen(path, "wb");
        if (!image) return FALSE;
        rdpGdi *gdi = probe->context.gdi;
        fprintf(image, "P6\n%d %d\n255\n", gdi->width, gdi->height);
        BYTE *row = malloc((size_t)gdi->width * 3);
        if (!row) { fclose(image); return FALSE; }
        for (INT32 y = 0; y < gdi->height; y++) {
            for (INT32 x = 0; x < gdi->width; x++) {
                const BYTE *pixel = gdi->primary_buffer + y * gdi->stride + x * 4;
                row[x * 3] = pixel[2]; row[x * 3 + 1] = pixel[1]; row[x * 3 + 2] = pixel[0];
            }
            if (fwrite(row, 3, (size_t)gdi->width, image) != (size_t)gdi->width) { free(row); fclose(image); return FALSE; }
        }
        free(row);
        if (fclose(image)) return FALSE;
    }
    /* A measured zero is useful evidence on a constrained bitmap link. */
    return TRUE;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    const char *user = getenv("NIRI_RDP_TEST_USERNAME");
    const char *pass = getenv("NIRI_RDP_TEST_PASSWORD");
    if (!user || !*user || !pass || !*pass) return 2;
    BOOL nla = getenv("NIRI_RDP_TEST_NLA") != NULL;
    BOOL gfx = getenv("NIRI_RDP_TEST_GFX") != NULL;
    BOOL login = getenv("NIRI_RDP_TEST_LOGIN_SCREEN") != NULL;
    if (login && nla) return 2;
    if (freerdp_register_addin_provider(freerdp_channels_load_static_addin_entry, 0) != 0)
        return 2;
    freerdp *rdp = freerdp_new();
    if (!rdp) return 2;
    rdp->ContextSize = sizeof(Probe);
    rdp->PostConnect = post_connect;
    rdp->PreConnect = pre_connect;
    rdp->LoadChannels = load_channels;
    rdp->AuthenticateEx = no_prompt;
    if (!freerdp_context_new(rdp)) { freerdp_free(rdp); return 2; }
    Probe *probe = (Probe *)rdp->context;
    const char *seconds = getenv("NIRI_RDP_TEST_PERFORMANCE_SECONDS");
    if (seconds) {
        char *end;
        unsigned long value = strtoul(seconds, &end, 10);
        if (*end || value < 1 || value > 120) return 2;
        probe->performance_seconds = (unsigned)value;
    }
    ((Probe *)rdp->context)->gfx_requested = gfx;
    ((Probe *)rdp->context)->expected_width = layouts[0][0];
    ((Probe *)rdp->context)->expected_height = layouts[0][1];
    ((Probe *)rdp->context)->pointer_x = layouts[0][0] / 2;
    ((Probe *)rdp->context)->pointer_y = layouts[0][1] / 2;
    if (probe->performance_seconds) {
        const char *width = getenv("NIRI_RDP_TEST_WIDTH"), *height = getenv("NIRI_RDP_TEST_HEIGHT");
        unsigned long w = width ? strtoul(width, NULL, 10) : 1920;
        unsigned long h = height ? strtoul(height, NULL, 10) : 1080;
        if (w < 1 || h < 1 || w > 4096 || h > 4096) return 2;
        probe->expected_width = (UINT32)w; probe->expected_height = (UINT32)h;
        probe->pointer_x = (UINT16)(w / 2); probe->pointer_y = (UINT16)(h / 2);
    }
    rdpSettings *s = rdp->context->settings;
    BOOL configured = freerdp_settings_set_string(s, FreeRDP_ServerHostname, "127.0.0.1") &&
        freerdp_settings_set_uint32(s, FreeRDP_ServerPort,
            getenv("NIRI_RDP_TEST_PORT") ? (UINT32)strtoul(getenv("NIRI_RDP_TEST_PORT"), NULL, 10) : 33991) &&
        freerdp_settings_set_string(s, FreeRDP_Username, login ? "" : user) &&
        freerdp_settings_set_string(s, FreeRDP_Password, login ? "" : pass) &&
        freerdp_settings_set_string(s, FreeRDP_Domain, "") &&
        freerdp_settings_set_bool(s, FreeRDP_NlaSecurity, nla) &&
        freerdp_settings_set_bool(s, FreeRDP_TlsSecurity, !nla) &&
        freerdp_settings_set_bool(s, FreeRDP_RdpSecurity, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_AutoLogonEnabled, !login) &&
        /* Only this disposable loopback probe accepts its ephemeral certificate. */
        freerdp_settings_set_bool(s, FreeRDP_IgnoreCertificate, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_SoftwareGdi, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_SupportGraphicsPipeline, gfx) &&
        freerdp_settings_set_bool(s, FreeRDP_SupportDynamicChannels, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_SupportDisplayControl, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_GfxH264, gfx) &&
        freerdp_settings_set_bool(s, FreeRDP_GfxAVC444, getenv("NIRI_RDP_TEST_AVC444") != NULL) &&
        freerdp_settings_set_bool(s, FreeRDP_GfxAVC444v2, getenv("NIRI_RDP_TEST_AVC444V2") != NULL) &&
        freerdp_settings_set_bool(s, FreeRDP_CompressionEnabled, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_BitmapCacheEnabled, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_RefreshRect, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_SuppressOutput, TRUE) &&
        freerdp_settings_set_uint32(s, FreeRDP_DesktopWidth, probe->expected_width) &&
        freerdp_settings_set_uint32(s, FreeRDP_DesktopHeight, probe->expected_height) &&
        freerdp_settings_set_uint32(s, FreeRDP_ColorDepth, 32) &&
        freerdp_settings_set_uint32(s, FreeRDP_TcpConnectTimeout, 5000);
    int result = 2;
    if (configured && freerdp_connect(rdp)) {
        Probe *probe = (Probe *)rdp->context;
        const BOOL resize = !probe->performance_seconds && !getenv("NIRI_RDP_TEST_HOLD") && !getenv("NIRI_RDP_TEST_CRASH") &&
            !getenv("NIRI_RDP_TEST_IME");
        const unsigned final_layout = resize ? sizeof(layouts) / sizeof(layouts[0]) - 1 : 0;
        ULONGLONG deadline = GetTickCount64() + (probe->performance_seconds ? probe->performance_seconds * 1000 : 20000);
        ULONGLONG next_performance_resize = GetTickCount64() + 1500;
        while ((probe->performance_seconds || probe->phase < 4 || probe->layout_index < final_layout) && GetTickCount64() < deadline) {
            if (probe->performance_seconds && probe->disp_ready &&
                getenv("NIRI_RDP_TEST_WORKLOAD") && !strcmp(getenv("NIRI_RDP_TEST_WORKLOAD"), "resize") &&
                GetTickCount64() >= next_performance_resize) {
                DISPLAY_CONTROL_MONITOR_LAYOUT monitor = {
                    .Flags = DISPLAY_CONTROL_MONITOR_PRIMARY,
                    .Width = probe->layout_index % 2 ? probe->expected_width : probe->expected_height,
                    .Height = probe->layout_index % 2 ? probe->expected_height : probe->expected_width,
                    .PhysicalWidth = 300, .PhysicalHeight = 200,
                    .DesktopScaleFactor = 100, .DeviceScaleFactor = 100
                };
                if (probe->disp->SendMonitorLayout(probe->disp, 1, &monitor) != CHANNEL_RC_OK) break;
                probe->layout_index++;
                next_performance_resize = GetTickCount64() + 1500;
            }
            if (login && probe->login_phase == 2 && GetTickCount64() >= probe->login_retry_at) {
                if (!login_text(rdp->context, pass) || !login_key(rdp->context, 28)) break;
                probe->login_phase = 3;
            }
            if (probe->awaiting_ack) {
                struct pollfd ack = {.fd = STDIN_FILENO, .events = POLLIN};
                if (poll(&ack, 1, 0) > 0) {
                    char line[32];
                    if (!fgets(line, sizeof(line), stdin) || strcmp(line, "continue\n") != 0) break;
                    probe->awaiting_ack = FALSE;
                }
            }
            unsigned previous_codecs = probe->avc_frames + probe->avc444_frames;
            update_codec_stats(probe);
            if (!probe->performance_seconds && !previous_codecs &&
                probe->avc_frames + probe->avc444_frames > 0 &&
                !end_paint(rdp->context)) break;
            if (probe->phase == 4 && probe->layout_index < final_layout && probe->disp_ready) {
                const unsigned next = probe->layout_index + 1;
                DISPLAY_CONTROL_MONITOR_LAYOUT monitor = {
                    .Flags = DISPLAY_CONTROL_MONITOR_PRIMARY,
                    .Width = layouts[next][0], .Height = layouts[next][1],
                    .PhysicalWidth = 300, .PhysicalHeight = 200,
                    .DesktopScaleFactor = 100, .DeviceScaleFactor = 100
                };
                if (probe->disp->SendMonitorLayout(probe->disp, 1, &monitor) != CHANNEL_RC_OK) break;
                probe->layout_index = next;
                probe->expected_width = layouts[next][0]; probe->expected_height = layouts[next][1];
                probe->pointer_x = layouts[next][0] / 2; probe->pointer_y = layouts[next][1] / 2;
                probe->size_verified = FALSE;
                probe->phase = 0;
            }
            if (probe->performance_seconds && !refresh_step(probe)) break;
            HANDLE events[64];
            DWORD n = freerdp_get_event_handles(rdp->context, events, 64);
            if (!n || WaitForMultipleObjects(n, events, FALSE, 100) == WAIT_FAILED ||
                !freerdp_check_event_handles(rdp->context)) break;
        }
        update_codec_stats(probe);
        if (probe->performance_seconds) {
            result = performance_receipt(probe) && (!getenv("NIRI_RDP_TEST_REFRESH") || probe->refresh_stage == 5) ? 0 : 2;
        } else if (probe->phase == 4 && probe->layout_index == final_layout && (!gfx || probe->avc_frames + probe->avc444_frames > 0)) {
            if (login) puts("NIRI_RDP_LOGIN_SCREEN_PAM_BINDING_OK");
            puts("NIRI_RDP_APP_KEY_POINTER_RELEASE_OK");
            if (resize) puts("NIRI_RDP_DYNAMIC_RESOLUTION_OK");
            if (gfx) printf("NIRI_RDP_AVC420_DECODED_FRAMES %u\n", probe->avc_frames);
            if (probe->avc444_frames) printf("NIRI_RDP_AVC444_DECODED_STREAMS %u\n", probe->avc444_frames);
            result = 0;
        }
        else fprintf(stderr, "app input/frame acknowledgement failed in phase %u layout %u, decoded %dx%d, expected %ux%u, DISP=%d\n",
                     probe->phase, probe->layout_index, probe->context.gdi->width, probe->context.gdi->height,
                     probe->expected_width, probe->expected_height, probe->disp_ready);
        if (!freerdp_disconnect(rdp)) result = 2;
    } else if (freerdp_get_last_error(rdp->context) == FREERDP_ERROR_AUTHENTICATION_FAILED ||
               freerdp_get_last_error(rdp->context) == FREERDP_ERROR_CONNECT_LOGON_FAILURE) {
        puts("RDP_AUTHENTICATION_REJECTED"); result = 4;
    } else fprintf(stderr, "connection failed: %s\n", freerdp_get_last_error_name(freerdp_get_last_error(rdp->context)));
    if (((Probe *)rdp->context)->initialized) gdi_free(rdp);
    freerdp_context_free(rdp); freerdp_free(rdp);
    return result;
}
