/* Synthetic native encode benchmark, including BGR -> YUV conversion.
 * Run from the repository root:
 *   cc -O3 scripts/encoder-bench.c -o /tmp/niri-rdp-encoder-bench \
 *     $(pkg-config --cflags --libs libavcodec libavutil libswscale)
 *   /tmp/niri-rdp-encoder-bench 0 1920 1080 60 12000000
 * Arguments: software (0 NVENC, 1 test x264), width, height, fps, bitrate.
 * 20 warm-up frames are excluded from 180 measured changing desktop frames.
 * A saved original video.c can be compared by compiling with -DBASELINE
 * and -DVIDEO_SOURCE='"/absolute/path/to/original-video.c"' (old ABI, 30 fps).
 */
#define _POSIX_C_SOURCE 200809L
#include <time.h>
#include <errno.h>
#ifndef VIDEO_SOURCE
#define VIDEO_SOURCE "../bridge/src/video.c"
#endif
#include VIDEO_SOURCE

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000.0 + t.tv_nsec / 1000000.0;
}
static int compare(const void *a, const void *b) {
    double d = *(const double *)a - *(const double *)b;
    return (d > 0) - (d < 0);
}
int main(int argc, char **argv) {
    int software = argc > 1 ? atoi(argv[1]) : 0;
    int width = argc > 2 ? atoi(argv[2]) : 1920;
    int height = argc > 3 ? atoi(argv[3]) : 1080;
    int fps = argc > 4 ? atoi(argv[4]) : 30;
    int bitrate = argc > 5 ? atoi(argv[5]) : 12000000;
    int frames = 180;
    if ((software != 0 && software != 1) || width < 1 || width > 4096 ||
        height < 1 || height > 4096 || fps < 1 || fps > 60 ||
        bitrate < 1000000 || bitrate > 100000000 || argc > 6) {
        fprintf(stderr, "usage: encoder-bench [0|1 [width [height [fps [bitrate]]]]]\n");
        return 1;
    }
    int stride = width * 4;
    uint8_t *pixels = calloc(height, stride);
    char error[256] = {0};
#ifdef BASELINE
    struct niri_video *v = niri_video_new(width, height, software, error, sizeof(error));
#else
    struct niri_video *v = niri_video_new(width, height, software, fps, bitrate, error, sizeof(error));
#endif
    if (!v) { fprintf(stderr, "%s\n", error); free(pixels); return 1; }
    double samples[180], sum = 0;
    size_t bytes = 0;
    for (int f = -20; f < frames; ++f) {
        for (int y = 0; y < height; ++y)
            for (int x = 0; x < width; ++x) {
                size_t i = (size_t)y * stride + x * 4;
                pixels[i] = x / 8;
                pixels[i + 1] = y / 4;
                pixels[i + 2] = f * 8;
                pixels[i + 3] = 255;
            }
        const uint8_t *packet;
        double start = now_ms();
        int n = niri_video_encode(v, pixels, stride, f == -20, &packet, error, sizeof(error));
        double elapsed = now_ms() - start;
        if (n < 0) { fprintf(stderr, "%s\n", error); return 1; }
        if (f >= 0) { samples[f] = elapsed; sum += elapsed; bytes += n; }
    }
    qsort(samples, frames, sizeof(double), compare);
    printf("%s %dx%d: average %.3f ms median %.3f ms p95 %.3f ms bytes %zu\n",
           software ? "x264" : "nvenc", width, height, sum / frames,
           samples[frames / 2], samples[frames * 95 / 100], bytes);
    niri_video_free(v);
    free(pixels);
    return 0;
}
