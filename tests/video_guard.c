#define _GNU_SOURCE
#include <assert.h>
#include <sys/mman.h>
#include <unistd.h>
#include <libavcodec/avcodec.h>
/* x264 also accepts NV12: exercise the hardware input layout without a GPU. */
#ifdef NIRI_VIDEO_TEST_NV12
#define AV_PIX_FMT_YUV420P AV_PIX_FMT_NV12
#endif
#include "../bridge/src/video.c"
int main(void) {
    av_log_set_level(AV_LOG_QUIET);
    const int heights[] = {1, 2, 17, 1080, 1081};
    const int large_widths[] = {1919, 1920, 4095, 4096};
    int cases = 0;
    for (int dimension = 0; dimension < 71; ++dimension) {
        int width = dimension < 67 ? dimension + 1 : large_widths[dimension - 67];
        for (int j = 0; j < 5; ++j) {
            int height = heights[j];
            int stride = width * 4;
            size_t page = sysconf(_SC_PAGESIZE);
            size_t length = (size_t)height * stride;
            size_t mapped = ((length + page - 1) / page + 1) * page;
            uint8_t *map = mmap(NULL, mapped, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            assert(map != MAP_FAILED);
            assert(mprotect(map + mapped - page, page, PROT_NONE) == 0);
            uint8_t *pixels = map + mapped - page - length;
            for (size_t k = 0; k < length; ++k) pixels[k] = k * 17;
            char error[256] = {0};
            struct niri_video *v = niri_video_new(width, height, 1, 60,
                                                 12000000, error, sizeof(error));
            assert(v);
            const uint8_t *packet;
            assert(niri_video_set_bitrate(NULL, 12000000) == -1);
            assert(niri_video_set_bitrate(v, 999999) == -1);
            assert(niri_video_set_bitrate(v, 100000001) == -1);
            assert(v->codec->bit_rate == 12000000);

            assert(niri_video_encode(v, pixels, stride, 1, &packet, error, sizeof(error)) > 0);
            if (v->rgb) assert(av_frame_is_writable(v->rgb));
            int nv12 = v->codec->pix_fmt == AV_PIX_FMT_NV12;
            for (int p = 0; p < (nv12 ? 2 : 3); ++p) {
                int w = p ? (width + 1) / 2 : width;
                int h = p ? (height + 1) / 2 : height;
                int pw = p ? v->codec->width / 2 : v->codec->width;
                int step = p && nv12 ? 2 : 1;
                if (step == 2) { w *= 2; pw *= 2; }
                int ph = p ? v->codec->height / 2 : v->codec->height;
                const uint8_t *data = v->frame->data[p];
                int linesize = v->frame->linesize[p];
                for (int y = 0; y < h; ++y)
                    for (int x = w; x < pw; ++x)
                        assert(data[y * linesize + x] == data[y * linesize + w - step + x % step]);
                for (int y = h; y < ph; ++y)
                    assert(memcmp(data + y * linesize, data + (h - 1) * linesize, pw) == 0);
            }
            /* Reuse the scaler after changing every pixel. Check full-range
               BT709 through all worker/slice boundaries, including large
               logical views backed by 16-aligned destination storage. */
            for (size_t k = 0; k < length; k += 4) {
                pixels[k] = pixels[k + 1] = 0;
                pixels[k + 2] = pixels[k + 3] = 255;
            }
            assert(niri_video_set_bitrate(v, 4000000) == 0);
            assert(v->codec->bit_rate == 4000000);
            assert(v->codec->rc_max_rate == 4000000);
            assert(v->codec->rc_buffer_size == 4000000 / 60);
            assert(niri_video_encode(v, pixels, stride, 1, &packet, error, sizeof(error)) > 0);
            if (v->rgb) assert(av_frame_is_writable(v->rgb));
            for (int p = 0; p < (nv12 ? 2 : 3); ++p) {
                int w = p ? (width + 1) / 2 : width;
                int h = p ? (height + 1) / 2 : height;
                if (p && nv12) w *= 2;
                for (int y = 0; y < h; ++y)
                    for (int x = 0; x < w; ++x) {
                        int expected = !p ? 54 : nv12 ? (x & 1 ? 255 : 99) : p == 1 ? 99 : 255;
                        int actual = v->frame->data[p][y * v->frame->linesize[p] + x];
                        assert(abs(actual - expected) <= 1);
                    }
            }
            if (width * height >= 640 * 480) {
                assert(niri_video_set_bitrate(v, 12000000) == 0);
                for (int white = 0; white <= 1; ++white) {
                    memset(pixels, white ? 255 : 0, length);
                    assert(niri_video_encode(v, pixels, stride, 1, &packet, error, sizeof(error)) > 0);
                    if (v->rgb) assert(av_frame_is_writable(v->rgb));
                    for (int p = 0; p < (nv12 ? 2 : 3); ++p) {
                        int w = p ? (width + 1) / 2 : width;
                        int h = p ? (height + 1) / 2 : height;
                        if (p && nv12) w *= 2;
                        int expected = p ? 128 : white ? 255 : 0;
                        for (int y = 0; y < h; ++y)
                            for (int x = 0; x < w; ++x)
                                assert(abs(v->frame->data[p][y * v->frame->linesize[p] + x] - expected) <= 1);
                    }
                }
            }
            niri_video_free(v);
            munmap(map, mapped);
            ++cases;
        }
    }
    printf("PASS: %d tightly packed guarded input cases; luma/chroma padding replicates edges\n", cases);
}
