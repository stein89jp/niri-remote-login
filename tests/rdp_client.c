#define _POSIX_C_SOURCE 200809L
/* Displayless integration client. All RDP framing, security and rendering are FreeRDP. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <freerdp/freerdp.h>
#include <freerdp/settings.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/input.h>
#include <freerdp/error.h>
#include <winpr/synch.h>
#include <winpr/sysinfo.h>

typedef struct {
    rdpContext context;
    unsigned phase;
    BOOL initialized;
} TestContext;

static BOOL begin_paint(rdpContext *context) {
    context->gdi->primary->hdc->hwnd->invalid->null = TRUE;
    context->gdi->primary->hdc->hwnd->ninvalid = 0;
    return TRUE;
}

static BOOL pixel_is(rdpGdi *gdi, unsigned x, unsigned y, BYTE b, BYTE g, BYTE r) {
    if (!gdi || gdi->width != 640 || gdi->height != 480 || !gdi->primary_buffer)
        return FALSE;
    const BYTE *pixel = gdi->primary_buffer + y * gdi->stride + x * 4;
    return pixel[0] == b && pixel[1] == g && pixel[2] == r;
}

static BOOL end_paint(rdpContext *context) {
    TestContext *test = (TestContext *)context;
    rdpGdi *gdi = context->gdi;
    if (test->phase == 0 && pixel_is(gdi, 20, 10, 35, 35, 55)) {
        /* Baseline decoded frame, then a harmless key on the synthetic backend. */
        if (!freerdp_input_send_keyboard_event(context->input, KBD_FLAGS_DOWN, 30) ||
            !freerdp_input_send_mouse_event(context->input, PTR_FLAGS_MOVE, 320, 240))
            return FALSE;
        test->phase = 1;
    } else if (test->phase == 1 && pixel_is(gdi, 20, 10, 30, 210, 30) &&
               pixel_is(gdi, 320, 100, 240, 240, 20)) {
        /* Key band and pointer cross prove both input paths reached the server. */
        if (!freerdp_input_send_keyboard_event(context->input, KBD_FLAGS_RELEASE, 30))
            return FALSE;
        test->phase = 2;
    } else if (test->phase == 2 && pixel_is(gdi, 20, 10, 35, 35, 55)) {
        test->phase = 3;
    }
    return TRUE;
}

static BOOL post_connect(freerdp *instance) {
    TestContext *test = (TestContext *)instance->context;
    /* Only CredSSP over TLS (HYBRID/HYBRID_EX), never plain RDP or TLS-only. */
    const UINT32 protocol = freerdp_settings_get_uint32(instance->context->settings,
                                                       FreeRDP_SelectedProtocol);
    if (protocol != 2 && protocol != 8)
        return FALSE;
    if (!gdi_init(instance, PIXEL_FORMAT_BGRA32))
        return FALSE;
    test->initialized = TRUE;
    instance->context->update->BeginPaint = begin_paint;
    instance->context->update->EndPaint = end_paint;
    return TRUE;
}

static BOOL refuse_prompt(freerdp *instance, char **user, char **password,
                          char **domain, rdp_auth_reason reason) {
    (void)instance; (void)user; (void)password; (void)domain; (void)reason;
    return FALSE;
}

int main(void) {
    const char *user = getenv("POC_USERNAME"), *password = getenv("POC_PASSWORD");
    if (!user || !*user || !password || !*password)
        return 2;
    freerdp *instance = freerdp_new();
    if (!instance)
        return 2;
    instance->ContextSize = sizeof(TestContext);
    instance->PostConnect = post_connect;
    instance->AuthenticateEx = refuse_prompt;
    if (!freerdp_context_new(instance)) {
        freerdp_free(instance);
        return 2;
    }
    rdpSettings *settings = instance->context->settings;
    BOOL configured = freerdp_settings_set_string(settings, FreeRDP_ServerHostname, "127.0.0.1") &&
        freerdp_settings_set_uint32(settings, FreeRDP_ServerPort, 33990) &&
        freerdp_settings_set_string(settings, FreeRDP_Username, user) &&
        freerdp_settings_set_string(settings, FreeRDP_Password, password) &&
        freerdp_settings_set_string(settings, FreeRDP_Domain, "") &&
        freerdp_settings_set_bool(settings, FreeRDP_NlaSecurity, TRUE) &&
        freerdp_settings_set_bool(settings, FreeRDP_TlsSecurity, FALSE) &&
        freerdp_settings_set_bool(settings, FreeRDP_RdpSecurity, FALSE) &&
        /* The managed loopback test server owns this ephemeral self-signed certificate. */
        freerdp_settings_set_bool(settings, FreeRDP_IgnoreCertificate, TRUE) &&
        freerdp_settings_set_bool(settings, FreeRDP_SoftwareGdi, TRUE) &&
        freerdp_settings_set_bool(settings, FreeRDP_SupportGraphicsPipeline, FALSE) &&
        freerdp_settings_set_bool(settings, FreeRDP_CompressionEnabled, FALSE) &&
        freerdp_settings_set_bool(settings, FreeRDP_BitmapCacheEnabled, FALSE) &&
        freerdp_settings_set_uint32(settings, FreeRDP_DesktopWidth, 640) &&
        freerdp_settings_set_uint32(settings, FreeRDP_DesktopHeight, 480) &&
        freerdp_settings_set_uint32(settings, FreeRDP_ColorDepth, 32) &&
        freerdp_settings_set_uint32(settings, FreeRDP_TcpConnectTimeout, 5000);
    int result = 2;
    if (configured && freerdp_connect(instance)) {
        TestContext *test = (TestContext *)instance->context;
        const ULONGLONG deadline = GetTickCount64() + 10000;
        while (test->phase < 3 && GetTickCount64() < deadline) {
            HANDLE events[64];
            const DWORD count = freerdp_get_event_handles(instance->context, events, 64);
            if (!count || WaitForMultipleObjects(count, events, FALSE, 100) == WAIT_FAILED ||
                !freerdp_check_event_handles(instance->context))
                break;
        }
        if (test->phase == 3) {
            puts("NLA_TLS_FRAME_KEY_POINTER_RELEASE_OK");
            result = 0;
        } else {
            fprintf(stderr, "synthetic frame exchange did not complete (phase %u)\n", test->phase);
        }
        if (!freerdp_disconnect(instance))
            result = 2;
    } else if (freerdp_get_last_error(instance->context) == FREERDP_ERROR_AUTHENTICATION_FAILED) {
        puts("NLA_AUTHENTICATION_REJECTED");
        result = 4;
    } else {
        fprintf(stderr, "connection failed: %s\n",
                freerdp_get_last_error_name(freerdp_get_last_error(instance->context)));
    }
    if (((TestContext *)instance->context)->initialized)
        gdi_free(instance);
    freerdp_context_free(instance);
    freerdp_free(instance);
    return result;
}
