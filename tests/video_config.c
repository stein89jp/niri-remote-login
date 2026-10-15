/* GOP configuration and descriptive native error text, without NVIDIA hardware:
 * the libx264 regression encoder shares the codec setup used for NVENC. */
#include <assert.h>
#include "../bridge/src/video.c"

static void expect_error(const char *error, const char *prefix) {
    if (strncmp(error, prefix, strlen(prefix)) != 0) {
        fprintf(stderr, "expected error \"%s...\", got \"%s\"\n", prefix, error);
        exit(1);
    }
}

int main(void) {
    av_log_set_level(AV_LOG_QUIET);
    char error[256];
    const uint8_t *data = NULL, *aux = NULL;
    int main_size = 0, aux_size = 0;

    /* Stable fixture/benchmark entry points keep the long default GOP. */
    memset(error, 'x', sizeof(error));
    struct niri_video *v = niri_video_new_config(64, 64, 1, 60, 12000000, 1, 0, 0,
                                                 error, sizeof(error));
    assert(v && error[0] == '\0');
    assert(v->codec->gop_size == NIRI_VIDEO_DEFAULT_GOP_SECONDS * 60);
    niri_video_free(v);

    /* AVC444 doubles the codec rate; an even GOP keeps IDRs on main views. */
    v = niri_video_new_encoder(64, 64, 1, 30, 12000000, 1, 0, 1, 5, error, sizeof(error));
    assert(v);
    assert(v->codec->gop_size == 5 * 30 * 2 && v->codec->gop_size % 2 == 0);
    int pixels_stride = 64 * 4;
    uint8_t *pixels = calloc(64 * 64, 4);
    assert(pixels);
    assert(niri_video_encode_pair(v, pixels, pixels_stride, 1, &data, &main_size,
                                  &aux, &aux_size, error, sizeof(error)) == 0);

    /* A failed color conversion reports its step instead of a bare -1. */
    struct SwsContext *valid = v->scale;
    v->scale = sws_getContext(64, 16, AV_PIX_FMT_BGR0, 64, 16, AV_PIX_FMT_YUV444P,
                              SWS_POINT, NULL, NULL, NULL);
    assert(v->scale);
    error[0] = '\0';
    assert(niri_video_encode_pair(v, pixels, pixels_stride, 0, &data, &main_size,
                                  &aux, &aux_size, error, sizeof(error)) == -1);
    expect_error(error, "convert AVC444 RGB to YUV444");
    sws_freeContext(v->scale);
    v->scale = valid;
    assert(niri_video_encode_pair(v, pixels, pixels_stride, 1, &data, &main_size,
                                  &aux, &aux_size, error, sizeof(error)) == 0);

    /* A context released by a failed GPU promotion is refused, not dereferenced. */
    avcodec_free_context(&v->codec);
    error[0] = '\0';
    assert(niri_video_encode_pair(v, pixels, pixels_stride, 1, &data, &main_size,
                                  &aux, &aux_size, error, sizeof(error)) == -1);
    expect_error(error, "H.264 encoder context released");
    assert(niri_video_set_bitrate(v, 4000000) == -1);
    assert(niri_video_set_rate(v, 4000000, 30) == -1);
    niri_video_free(v);

    v = niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0, 1, error, sizeof(error));
    assert(v && v->codec->gop_size == 60);
    avcodec_free_context(&v->codec);
    error[0] = '\0';
    assert(niri_video_encode(v, pixels, pixels_stride, 1, &data, error, sizeof(error)) == -1);
    expect_error(error, "H.264 encoder context released");
    niri_video_free(v);

    /* The GOP knob is bounded at the ABI as well as in the frontend. */
    assert(!niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0, 0, error, sizeof(error)));
    expect_error(error, "invalid H.264 dimensions, framerate, bitrate or GOP");
    assert(!niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0,
                                   NIRI_VIDEO_MAX_GOP_SECONDS + 1, error, sizeof(error)));
    expect_error(error, "invalid H.264");
    v = niri_video_new_encoder(64, 64, 1, 60, 12000000, 1, 0, 0, NIRI_VIDEO_MAX_GOP_SECONDS,
                               error, sizeof(error));
    assert(v && v->codec->gop_size == NIRI_VIDEO_MAX_GOP_SECONDS * 60);
    niri_video_free(v);

    double conversion = -1, encode = -1;
    niri_video_timings(NULL, &conversion, &encode);
    assert(conversion == 0 && encode == 0);
    free(pixels);
    puts("PASS: GOP bounds/defaults and descriptive native encoder errors");
    return 0;
}
