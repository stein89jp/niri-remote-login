/* Deterministic synthetic desktop workloads, used only in the private lab. */
#ifndef NIRI_RDP_PERFORMANCE_PATTERN_H
#define NIRI_RDP_PERFORMANCE_PATTERN_H
#include <stdint.h>
#include <string.h>
static void performance_text(uint32_t *pixels, unsigned width, unsigned height,
                             unsigned left, unsigned top, unsigned count,
                             uint32_t foreground, uint32_t background) {
    static const uint8_t letters[8][7] = {
        {17,25,21,19,17,17,17}, {14,4,4,4,4,4,14},
        {30,17,17,30,20,18,17}, {14,4,4,4,4,4,14},
        {0,0,0,0,0,0,0}, {30,17,17,30,20,18,17},
        {30,17,17,17,17,17,30}, {30,17,17,30,16,16,16},
    };
    for (unsigned i=0; i<count; i++)
        for (unsigned y=0; y<7; y++)
            for (unsigned x=0; x<5; x++) {
                unsigned px=left+i*7+x, py=top+y;
                if (px<width && py<height)
                    pixels[(size_t)py*width+px] = letters[i%8][y]&(1u<<(4-x)) ? foreground : background;
            }
}
static void performance_pattern(uint32_t *pixels, unsigned width, unsigned height,
                                unsigned frame, const char *workload) {
    if (!workload) return;
    const uint32_t background = 0xfff0f0f0;
    for (unsigned y=0; y<height; y++)
        for (unsigned x=0; x<width; x++) {
            uint32_t color=background;
            if (!strcmp(workload,"video"))
                color=0xff000000|(((x+frame*5)&255)<<16)|(((y+frame*3)&255)<<8)|((x+y+frame*7)&255);
            else if (!strcmp(workload,"scroll"))
                color=((y+frame*4)%32<16)?0xfff8f8f8:0xffd0d0d0;
            else if (!strcmp(workload,"window")) {
                unsigned left=(frame*5)%(width>300?width-300:1), top=(frame*3)%(height>200?height-200:1);
                if (x>=left && x<left+300 && y>=top && y<top+200) color=0xff4050b0;
            }
            pixels[(size_t)y*width+x]=color;
        }
    unsigned offset=!strcmp(workload,"scroll")?frame*4%32:0;
    for (unsigned y=16; y+7<height; y+=32) {
        performance_text(pixels,width,height,16,y+offset,96,0xff202020,background);
        if (!strcmp(workload,"colored-text"))
            performance_text(pixels,width,height,width/2,y,96,(y/32)%2?0xffff2020:0xff2020ff,background);
    }
    if (!strcmp(workload,"typing"))
        performance_text(pixels,width,height,16,4,(frame/6)%64,0xff000000,background);
}
#endif
