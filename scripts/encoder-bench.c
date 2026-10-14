/* Deterministic desktop codec benchmark. Conversion/encode only: capture,
 * network, client decoding and physical presentation are separate receipts.
 * cc -O3 scripts/encoder-bench.c -o /tmp/niri-rdp-encoder-bench \
 *   $(pkg-config --cflags --libs libavcodec libavutil libswscale)
 * encoder-bench software width height fps bitrate preset aq workload avc444 output
 * Defaults retain p1/AQ-off/AVC420. Run matched profiles before changing defaults.
 * Workloads: idle, typing, scroll, color-text, move, video, resize. */
#define _POSIX_C_SOURCE 200809L
#include <time.h>
#include <errno.h>
#include <sys/resource.h>
#ifndef VIDEO_SOURCE
#define VIDEO_SOURCE "../bridge/src/video.c"
#endif
#include VIDEO_SOURCE
#define FRAMES 180
static double now_ms(void) {return video_now_ms();}
static int compare(const void *a,const void *b) {double d=*(const double *)a-*(const double *)b;return (d>0)-(d<0);}
static double cpu_ms(void) {struct rusage u;getrusage(RUSAGE_SELF,&u);return (u.ru_utime.tv_sec+u.ru_stime.tv_sec)*1000.0+(u.ru_utime.tv_usec+u.ru_stime.tv_usec)/1000.0;}
static void pixels_for(uint8_t *pixels,int w,int h,int frame,const char *workload) {
    int f=frame<0?0:frame;
    for(int y=0;y<h;++y) for(int x=0;x<w;++x) {
        uint8_t *p=pixels+((size_t)y*w+x)*4;
        int scroll=!strcmp(workload,"scroll")?f*5:0;
        int glyph=((x%9<5)&&((y+scroll)%15<9)&&(((x/9+(y+scroll)/15+f/20)>>((x%5)%3))&1));
        int visible=!strcmp(workload,"typing")? x/9<f%120 : 1;
        p[0]=p[1]=p[2]=(glyph&&visible)?220:24;
        if(!strcmp(workload,"color-text")&&glyph) {p[0]=(x/9)%3==0?255:0;p[1]=(x/9)%3==1?255:0;p[2]=(x/9)%3==2?255:0;}
        if(!strcmp(workload,"idle")) {p[0]=p[1]=p[2]=(x%64<2||y%64<2)?100:24;}
        if(!strcmp(workload,"move")) {int inside=x>f*7%w&&x<f*7%w+w/3&&y>h/4&&y<h*3/4;p[0]=inside?220:24;p[1]=inside?80:24;p[2]=inside?40:24;}
        if(!strcmp(workload,"video")) {p[0]=(x+f*8)/8;p[1]=(y+f*4)/4;p[2]=f*8;}
        p[3]=255;
    }
}
int main(int argc,char **argv) {
    int software=argc>1?atoi(argv[1]):0,w=argc>2?atoi(argv[2]):1920,h=argc>3?atoi(argv[3]):1080;
    int fps=argc>4?atoi(argv[4]):60,bitrate=argc>5?atoi(argv[5]):12000000;
    int preset=argc>6?atoi(argv[6]):1,aq=argc>7?atoi(argv[7]):0;
    const char *workload=argc>8?argv[8]:"video";
    int avc444=argc>9?atoi(argv[9]):0;
    if((software!=0&&software!=1)||w<1||w>4096||h<1||h>4096||fps<1||fps>60||
       bitrate<1000000||bitrate>100000000||preset<1||preset>7||(aq!=0&&aq!=1)||(avc444!=0&&avc444!=1)||argc>11||
       (strcmp(workload,"idle")&&strcmp(workload,"typing")&&strcmp(workload,"scroll")&&strcmp(workload,"color-text")&&strcmp(workload,"move")&&strcmp(workload,"video")&&strcmp(workload,"resize")&&strcmp(workload,"refinement"))) {
        fprintf(stderr,"usage: encoder-bench [software width height fps bitrate preset aq idle|typing|scroll|color-text|move|video|resize avc444 output.h264]\n");return 1;
    }
    FILE *stream=argc>10?fopen(argv[10],"wb"):NULL;
    if(argc>10&&!stream) {perror("stream");return 1;}
    uint8_t *pixels=malloc((size_t)w*h*4);if(!pixels)return 1;
    char error[256]={0};
    struct niri_video *v=niri_video_new_config(w,h,software,fps,bitrate,preset,aq,avc444,error,sizeof(error));
    if(!v) {fprintf(stderr,"%s\n",error);free(pixels);if(stream)fclose(stream);return 1;}
    av_log_set_level(AV_LOG_ERROR);
    double samples[FRAMES],conversion[FRAMES],encoding[FRAMES],sum=0,cpu_start=cpu_ms(),wall_start=now_ms();
    size_t bytes=0,max_bytes=0;
    for(int f=-20;f<FRAMES;++f) {
        int width=w,height=h;
        if(!strcmp(workload,"resize")&&(f+20)%30==0) {
            niri_video_free(v);width=w-(((f+20)/30)%2);height=h-(((f+20)/30)%2);
            v=niri_video_new_config(width,height,software,fps,bitrate,preset,aq,avc444,error,sizeof(error));
            if(!v)goto failure;
        } else {width=v->width;height=v->height;}
        const char *drawing=!strcmp(workload,"refinement")?(f>=178?"color-text":"video"):workload;
        pixels_for(pixels,width,height,f,drawing);
        const uint8_t *main_data,*aux_data=NULL;int main_size,aux_size=0;
        double start=now_ms();
        if(avc444) {
            if(niri_video_encode_pair(v,pixels,width*4,f==-20||(!strcmp(workload,"resize")&&(f+20)%30==0),&main_data,&main_size,&aux_data,&aux_size,error,sizeof(error))<0)goto failure;
        }else{main_size=niri_video_encode(v,pixels,width*4,f==-20,&main_data,error,sizeof(error));if(main_size<0)goto failure;}
        double elapsed=now_ms()-start;
        if(stream) {if(fwrite(main_data,1,main_size,stream)!=(size_t)main_size)goto failure;if(aux_size&&fwrite(aux_data,1,aux_size,stream)!=(size_t)aux_size)goto failure;}
        if(f>=0) {samples[f]=elapsed;conversion[f]=v->convert_ms;encoding[f]=v->encode_ms;sum+=elapsed;bytes+=main_size+aux_size;if((size_t)(main_size+aux_size)>max_bytes)max_bytes=main_size+aux_size;}
    }
    double cpu=cpu_ms()-cpu_start,wall=now_ms()-wall_start;
    qsort(samples,FRAMES,sizeof(double),compare);qsort(conversion,FRAMES,sizeof(double),compare);qsort(encoding,FRAMES,sizeof(double),compare);
    printf("{\"scope\":\"native_conversion_encode\",\"physical_presentation_measured\":false,\"encoder\":\"%s\",\"width\":%d,\"height\":%d,\"fps_target\":%d,\"bitrate_ceiling\":%d,\"preset\":%d,\"spatial_aq\":%d,\"codec\":\"%s\",\"workload\":\"%s\",\"frames\":%d,\"warmup_frames\":20,\"mean_ms\":%.3f,\"p50_ms\":%.3f,\"p95_ms\":%.3f,\"max_ms\":%.3f,\"conversion_p95_ms\":%.3f,\"encode_p95_ms\":%.3f,\"payload_bytes\":%zu,\"max_frame_bytes\":%zu,\"nominal_payload_mbps\":%.3f,\"process_cpu_ms\":%.3f,\"wall_ms\":%.3f,\"avcodec_version\":%u}\n",
           software?"x264":"nvenc",w,h,fps,bitrate,preset,aq,avc444?"avc444":"avc420",workload,FRAMES,sum/FRAMES,samples[FRAMES/2],samples[FRAMES*95/100],samples[FRAMES-1],conversion[FRAMES*95/100],encoding[FRAMES*95/100],bytes,max_bytes,(double)bytes*8*fps/FRAMES/1000000,cpu,wall,avcodec_version());
    if(stream) {
        size_t name_length=strlen(argv[10])+32;char *name=malloc(name_length);
        if(!name)goto failure;
        snprintf(name,name_length,"%s.reference.bgra",argv[10]);FILE *reference=fopen(name,"wb");free(name);
        if(!reference)goto failure;
        size_t n=(size_t)v->width*v->height*4;
        if(fwrite(pixels,1,n,reference)!=n) {fclose(reference);goto failure;}fclose(reference);
    }
    niri_video_free(v);free(pixels);if(stream)fclose(stream);return 0;
failure:
    fprintf(stderr,"%s\n",error);niri_video_free(v);free(pixels);if(stream)fclose(stream);return 1;
}
