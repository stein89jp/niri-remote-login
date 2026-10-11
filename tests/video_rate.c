/* Optional hardware regression: one live NVENC context across rate changes. */
#include <assert.h>
#include "../bridge/src/video.c"

int main(int argc, char **argv) {
    assert(argc == 2);
    char error[256] = {0};
    const int width = 640, height = 360, frames = 60;
    struct niri_video *video = niri_video_new(width, height, 0, 60, 12000000,
                                             error, sizeof(error));
    if (!video) { fprintf(stderr, "%s\n", error); return 1; }
    FILE *stream = fopen(argv[1], "wb");
    assert(stream);
    uint8_t *pixels = malloc((size_t)width * height * 4);
    assert(pixels);
    const int rates[] = {12000000, 2000000, 8000000};
    size_t sizes[3] = {0};
    uint32_t random = 0x1badbeef;
    for (int phase = 0; phase < 3; ++phase) {
        assert(niri_video_set_bitrate(video, rates[phase]) == 0);
        for (int frame = 0; frame < frames; ++frame) {
            for (int i = 0; i < width * height * 4; ++i) {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                pixels[i] = random;
            }
            const uint8_t *packet = NULL;
            int length = niri_video_encode(video, pixels, width * 4,
                                          phase == 0 && frame == 0,
                                          &packet, error, sizeof(error));
            if (length < 1) { fprintf(stderr, "%s\n", error); return 1; }
            assert(fwrite(packet, 1, length, stream) == (size_t)length);
            sizes[phase] += length;
        }
    }
    fclose(stream);
    free(pixels);
    niri_video_free(video);
    printf("NVENC phase bytes: %zu %zu %zu\n", sizes[0], sizes[1], sizes[2]);
    assert(sizes[1] < sizes[0] / 2);
    assert(sizes[2] > sizes[1] * 3 / 2);
    puts("PASS: live NVENC bitrate decreases and recovers");
    return 0;
}
