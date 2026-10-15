#define _POSIX_C_SOURCE 200809L
/* Displayless FreeRDP client for the microphone lab: playback (rdpsnd) and
 * microphone (audin) both use FreeRDP's PulseAudio backends against the
 * client-side private graph named by PULSE_SERVER. FreeRDP owns all RDP
 * protocol and security. Disposable loopback use only; it ignores the
 * certificate of the ephemeral lab server.
 *
 * Environment: NIRI_RDP_TEST_USERNAME, NIRI_RDP_TEST_PASSWORD (required),
 * NIRI_RDP_TEST_PORT (default 33991), NIRI_RDP_TEST_SPEAKER and
 * NIRI_RDP_TEST_MICROPHONE (client-side PulseAudio device names; an empty
 * NIRI_RDP_TEST_MICROPHONE disables microphone redirection).
 * It runs until SIGTERM/SIGINT, then disconnects cleanly. */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <freerdp/freerdp.h>
#include <freerdp/client/channels.h>
#include <freerdp/client/cmdline.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/settings.h>
#include <winpr/synch.h>

static volatile sig_atomic_t stopping;

static void stop(int signal_number) {
    (void)signal_number;
    stopping = 1;
}

static BOOL load_channels(freerdp *rdp) {
    return freerdp_client_load_addins(rdp->context->channels, rdp->context->settings);
}

static BOOL post_connect(freerdp *rdp) {
    return gdi_init(rdp, PIXEL_FORMAT_BGRX32);
}

static BOOL no_prompt(freerdp *rdp, char **user, char **pass, char **domain, rdp_auth_reason reason) {
    (void)rdp; (void)user; (void)pass; (void)domain; (void)reason;
    return TRUE;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    const char *user = getenv("NIRI_RDP_TEST_USERNAME");
    const char *pass = getenv("NIRI_RDP_TEST_PASSWORD");
    const char *speaker = getenv("NIRI_RDP_TEST_SPEAKER");
    const char *microphone = getenv("NIRI_RDP_TEST_MICROPHONE");
    const char *port = getenv("NIRI_RDP_TEST_PORT");
    if (!user || !*user || !pass || !*pass || !speaker || !*speaker || !microphone) return 2;
    struct sigaction action = {.sa_handler = stop};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGTERM, &action, NULL) || sigaction(SIGINT, &action, NULL)) return 2;
    if (freerdp_register_addin_provider(freerdp_channels_load_static_addin_entry, 0) != 0) return 2;
    freerdp *rdp = freerdp_new();
    if (!rdp) return 2;
    rdp->PostConnect = post_connect;
    rdp->LoadChannels = load_channels;
    rdp->AuthenticateEx = no_prompt;
    if (!freerdp_context_new(rdp)) { freerdp_free(rdp); return 2; }
    rdpSettings *s = rdp->context->settings;
    char sound[256], mic[256];
    snprintf(sound, sizeof(sound), "/sound:sys:pulse,dev:%s", speaker);
    snprintf(mic, sizeof(mic), "/microphone:sys:pulse,dev:%s", microphone);
    char *argv[] = {"rdp-audio-client", "/v:127.0.0.1", "/audio-mode:0", sound, mic, NULL};
    int argc = *microphone ? 5 : 4;
    BOOL configured =
        freerdp_client_settings_parse_command_line_arguments(s, argc, argv, FALSE) == 0 &&
        freerdp_settings_set_string(s, FreeRDP_ServerHostname, "127.0.0.1") &&
        freerdp_settings_set_uint32(s, FreeRDP_ServerPort, port ? (UINT32)strtoul(port, NULL, 10) : 33991) &&
        freerdp_settings_set_string(s, FreeRDP_Username, user) &&
        freerdp_settings_set_string(s, FreeRDP_Password, pass) &&
        freerdp_settings_set_string(s, FreeRDP_Domain, "") &&
        freerdp_settings_set_bool(s, FreeRDP_NlaSecurity, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_TlsSecurity, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_RdpSecurity, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_AutoLogonEnabled, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_IgnoreCertificate, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_SoftwareGdi, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_SupportGraphicsPipeline, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_SupportDynamicChannels, TRUE) &&
        freerdp_settings_set_bool(s, FreeRDP_CompressionEnabled, FALSE) &&
        freerdp_settings_set_bool(s, FreeRDP_BitmapCacheEnabled, FALSE) &&
        freerdp_settings_set_uint32(s, FreeRDP_DesktopWidth, 800) &&
        freerdp_settings_set_uint32(s, FreeRDP_DesktopHeight, 600) &&
        freerdp_settings_set_uint32(s, FreeRDP_ColorDepth, 32) &&
        freerdp_settings_set_uint32(s, FreeRDP_TcpConnectTimeout, 5000);
    int result = 2;
    if (configured && freerdp_connect(rdp)) {
        printf("NIRI_RDP_AUDIO_CLIENT_CONNECTED\n");
        result = 0;
        while (!stopping && !freerdp_shall_disconnect_context(rdp->context)) {
            HANDLE handles[MAXIMUM_WAIT_OBJECTS];
            DWORD count = freerdp_get_event_handles(rdp->context, handles, ARRAYSIZE(handles));
            if (!count) { result = 3; break; }
            if (WaitForMultipleObjects(count, handles, FALSE, 100) == WAIT_FAILED) { result = 3; break; }
            if (!freerdp_check_event_handles(rdp->context)) {
                if (!stopping) result = 4;
                break;
            }
        }
        freerdp_disconnect(rdp);
        printf("NIRI_RDP_AUDIO_CLIENT_DISCONNECTED %d\n", result);
    } else {
        fprintf(stderr, "connect failed: 0x%08x\n", freerdp_get_last_error(rdp->context));
    }
    freerdp_context_free(rdp);
    freerdp_free(rdp);
    return result;
}
