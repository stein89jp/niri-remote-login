/* Displayless interoperability with the actual FreeRDP client DSP. */
#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <freerdp/codec/dsp.h>
#include <freerdp/freerdp.h>

struct niri_audio_aac;
struct niri_audio_aac *niri_audio_aac_new(char *error, size_t length);
void niri_audio_aac_free(struct niri_audio_aac *encoder);
int niri_audio_aac_encode(struct niri_audio_aac *encoder, const uint8_t *pcm,
                          size_t length, const uint8_t **data,
                          char *error, size_t error_length);
int niri_audio_aac_receive(struct niri_audio_aac *encoder, const uint8_t **data,
                           char *error, size_t error_length);

enum { INPUT_FRAMES = 960, INPUT_BYTES = INPUT_FRAMES * 4, INPUT_BLOCKS = 240 };

static const AUDIO_FORMAT aac_format = {
    .wFormatTag = WAVE_FORMAT_AAC_MS,
    .nChannels = 2,
    .nSamplesPerSec = 44100,
    .nAvgBytesPerSec = 12000,
    .nBlockAlign = 4,
    .wBitsPerSample = 16,
    .cbSize = 0,
    .data = NULL,
};

static const AUDIO_FORMAT pcm_format = {
    .wFormatTag = WAVE_FORMAT_PCM,
    .nChannels = 2,
    .nSamplesPerSec = 48000,
    .nAvgBytesPerSec = 192000,
    .nBlockAlign = 4,
    .wBitsPerSample = 16,
    .cbSize = 0,
    .data = NULL,
};

static int16_t read_sample(const BYTE *data) {
    return (int16_t)((uint16_t)data[0] | (uint16_t)data[1] << 8);
}

static void write_sample(uint8_t *data, int16_t sample) {
    data[0] = (uint8_t)sample;
    data[1] = (uint8_t)((uint16_t)sample >> 8);
}

static double tone_fraction(const BYTE *data, size_t frames, int channel, int frequency) {
    const double pi = acos(-1.0);
    double energy = 0, cosine = 0, sine = 0;
    for (size_t frame = 0; frame < frames; ++frame) {
        double sample = read_sample(data + frame * 4 + channel * 2);
        double phase = 2 * pi * frequency * frame / aac_format.nSamplesPerSec;
        energy += sample * sample;
        cosine += sample * cos(phase);
        sine += sample * sin(phase);
        assert(fabs(sample) < 26000);
    }
    assert(energy / frames > 1000000 && "silence is not successful decoding");
    return 2 * (cosine * cosine + sine * sine) / (frames * energy);
}

static void test_formats(void) {
    wStream *wire = Stream_New(NULL, 64);
    assert(wire);
    assert(audio_format_write(wire, &aac_format));
    assert(Stream_GetPosition(wire) == 18 && "AAC_MS has no format extension");
    Stream_SealLength(wire);
    Stream_ResetPosition(wire);
    AUDIO_FORMAT read = {0};
    assert(audio_format_read(wire, &read));
    assert(audio_format_compatible(&aac_format, &read));
    assert(read.nAvgBytesPerSec == 12000 && read.nBlockAlign == 4);
    assert(read.wBitsPerSample == 16 && read.cbSize == 0 && read.data == NULL);
    assert(freerdp_dsp_supports_format(&read, FALSE) && "GNOME AAC_MS offer must be accepted");
    assert(freerdp_dsp_supports_format(&pcm_format, FALSE) && "PCM fallback must be accepted");
    audio_format_free(&read);
    Stream_Free(wire, TRUE);

    /* The prepared 3.32.1 build disables experimental DSP codecs. This is
       its real filter, rather than a duplicated list in our test client. */
    AUDIO_FORMAT adpcm = pcm_format;
    adpcm.wFormatTag = WAVE_FORMAT_ADPCM;
    adpcm.nAvgBytesPerSec = 48000 * 1024 / 1012;
    adpcm.nBlockAlign = 1024;
    adpcm.wBitsPerSample = 4;
    assert(!freerdp_dsp_supports_format(&adpcm, FALSE));
    puts("PASS: FreeRDP accepts exact AAC_MS and PCM formats; MS ADPCM is filtered");
}

static void test_aac_client_decode(void) {
    const double pi = acos(-1.0);
    char error[256] = {0};
    for (int reconnect = 0; reconnect < 2; ++reconnect) {
        struct niri_audio_aac *encoder = niri_audio_aac_new(error, sizeof(error));
        assert(encoder);
        FREERDP_DSP_CONTEXT *client = freerdp_dsp_context_new(FALSE);
        wStream *decoded = Stream_New(NULL, 8192);
        assert(client && decoded);
        assert(freerdp_dsp_context_reset(client, &aac_format, 1024));
        uint8_t pcm[INPUT_BYTES];
        int packets = 0, played = 0, quality = 0, dropped = 0, last_drop = -10;
        size_t samples = 0, encoded_bytes = 0;
        for (int block = 0; block < INPUT_BLOCKS; ++block) {
            for (int frame = 0; frame < INPUT_FRAMES; ++frame) {
                double time = (block * INPUT_FRAMES + frame) / 48000.0;
                write_sample(pcm + frame * 4, (int16_t)(10000 * sin(2 * pi * 440 * time)));
                write_sample(pcm + frame * 4 + 2, (int16_t)(7000 * sin(2 * pi * 997 * time)));
            }
            const uint8_t *packet = NULL;
            int length = niri_audio_aac_encode(encoder, pcm, sizeof(pcm),
                                               &packet, error, sizeof(error));
            if (length < 0) fprintf(stderr, "AAC encoding failed: %s\n", error);
            assert(length >= 0);
            if (length == 0) continue;
            assert(packet && length < 2048);
            assert(!(packet[0] == 0xff && (packet[1] & 0xf0) == 0xf0) && "packets must be raw AAC");
            encoded_bytes += (size_t)length;
            ++packets;
            if (reconnect && packets % 17 == 8) {
                last_drop = packets;
                ++dropped;
                continue;
            }
            Stream_ResetPosition(decoded);
            assert(freerdp_dsp_decode(client, &aac_format, packet, (size_t)length, decoded)
                && "actual FreeRDP client DSP must decode raw AAC packets");
            size_t bytes = Stream_GetPosition(decoded);
            assert(bytes == 1024 * 4 && "each packet must yield 1024 stereo S16 frames at 44.1 kHz");
            samples += bytes / 4;
            ++played;
            /* Overlap after a missing AAC packet may affect its successor.
               Intelligible stereo must recover within two packets (46 ms). */
            if (packets > 4 && packets - last_drop > 2) {
                assert(tone_fraction(Stream_Buffer(decoded), 1024, 0, 440) > 0.90);
                assert(tone_fraction(Stream_Buffer(decoded), 1024, 1, 997) > 0.90);
                ++quality;
            }
            assert(niri_audio_aac_receive(encoder, &packet, error, sizeof(error)) == 0);
        }
        int ideal_packets = INPUT_BLOCKS * 882 / 1024;
        assert(packets >= ideal_packets - 2 && packets <= ideal_packets);
        assert(played == packets - dropped && samples == (size_t)played * 1024);
        assert(quality > 150);
        double duration = packets * 1024 / 44100.0;
        assert(encoded_bytes * 8 / duration < 140000);
        printf("PASS: FreeRDP raw AAC decode: reconnect=%d packets=%d dropped=%d "
               "stereo_frames=%zu quality_packets=%d bitrate=%.0f bps\n",
               reconnect, packets, dropped, samples, quality, encoded_bytes * 8 / duration);
        Stream_Free(decoded, TRUE);
        freerdp_dsp_context_free(client);
        niri_audio_aac_free(encoder);
    }
}

int main(void) {
    printf("FreeRDP client DSP version: %s\n", freerdp_get_version_string());
    test_formats();
    test_aac_client_decode();
    return 0;
}
