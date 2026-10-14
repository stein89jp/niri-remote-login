/* Validate the RDP AVC444 view layout independently of the native encoder:
 * recover every full-resolution chroma sample from main and auxiliary views,
 * then decode both views with ONE H.264 decoder and check their reference chain. */
#include <assert.h>
#include <math.h>
#include <libavcodec/avcodec.h>
#include "../bridge/src/video.c"
static uint8_t chroma(const AVFrame *f,int p,int x,int y) {
    if(f->format==AV_PIX_FMT_NV12) return f->data[1][y*f->linesize[1]+2*x+p-1];
    return f->data[p][y*f->linesize[p]+x];
}
int main(void) {
    av_log_set_level(AV_LOG_ERROR);
    int decoded=0,cases=0;
    for(int size=0;size<4;++size) {
        int widths[]={1,17,63,640},heights[]={17,1,61,368};
        int w=widths[size],h=heights[size],stride=w*4;
        char error[256]={0};
        struct niri_video *v=niri_video_new_config(w,h,1,30,12000000,1,0,1,error,sizeof(error));
        assert(v);
        uint8_t *pixels=malloc((size_t)stride*h);
        const AVCodec *decoder=avcodec_find_decoder(AV_CODEC_ID_H264);
        AVCodecContext *context=avcodec_alloc_context3(decoder);assert(context);
        context->thread_count=1;assert(avcodec_open2(context,decoder,NULL)==0);
        AVFrame *view=av_frame_alloc();AVPacket *packet=av_packet_alloc();
        for(int f=0;f<8;++f) {
            for(int y=0;y<h;++y) for(int x=0;x<w;++x) {
                uint8_t *p=pixels+(size_t)y*stride+4*x;
                p[0]=(x%3)?0:255;p[1]=(y%3)?0:255;p[2]=(x+y+f)%2?255:0;p[3]=255;
            }
            const uint8_t *main_data,*aux_data;int main_size,aux_size;
            assert(niri_video_encode_pair(v,pixels,stride,f==0,&main_data,&main_size,&aux_data,&aux_size,error,sizeof(error))==0);
            /* RGB->444 must retain each pixel's chroma before packing. The
             * default swscale fast-bilinear path can generate half-width RGB
             * chroma even for a 444 destination; decoder success misses this. */
            for(int y=0;y<h;++y)for(int x=0;x<w;++x)for(int p=0;p<3;++p) {
                const uint8_t *rgb=pixels+(size_t)y*stride+4*x;
                double expected=p==0?.2126*rgb[2]+.7152*rgb[1]+.0722*rgb[0]:p==1?
                    -.114572*rgb[2]-.385428*rgb[1]+.5*rgb[0]+128:
                    .5*rgb[2]-.454153*rgb[1]-.045847*rgb[0]+128;
                if(expected<0)expected=0;
                if(expected>255)expected=255;
                int actual=v->full_chroma->data[p][y*v->full_chroma->linesize[p]+x];
                assert(abs(actual-(int)(expected+.5))<=1);
            }
            /* Layout reconstruction is checked before codec loss. Rounding the
             * 2x2 average adds at most two levels at even/even locations. */
            for(int p=1;p<=2;++p) for(int y=0;y<v->codec->height;++y) for(int x=0;x<v->codec->width;++x) {
                int expected=v->full_chroma->data[p][y*v->full_chroma->linesize[p]+x];
                int ay=(y/16)*16+(y%16)/2+8*(p-1),recovered;
                if(y&1) recovered=v->auxiliary->data[0][ay*v->auxiliary->linesize[0]+x];
                else if(x&1) recovered=chroma(v->auxiliary,p,x/2,y/2);
                else recovered=4*chroma(v->frame,p,x/2,y/2)-chroma(v->auxiliary,p,x/2,y/2)
                        -v->auxiliary->data[0][ay*v->auxiliary->linesize[0]+x]
                        -v->auxiliary->data[0][ay*v->auxiliary->linesize[0]+x+1];
                assert(abs(expected-recovered)<=2);
            }
            for(int auxiliary=0;auxiliary<2;++auxiliary) {
                assert(av_new_packet(packet,auxiliary?aux_size:main_size)==0);
                memcpy(packet->data,auxiliary?aux_data:main_data,packet->size);
                assert(avcodec_send_packet(context,packet)==0);
                assert(avcodec_receive_frame(context,view)==0);
                assert(view->width==v->codec->width&&view->height==v->codec->height);
                assert(view->color_range==AVCOL_RANGE_JPEG&&view->colorspace==AVCOL_SPC_BT709);
                av_frame_unref(view);av_packet_unref(packet);++decoded;
            }
        }
        av_packet_free(&packet);av_frame_free(&view);avcodec_free_context(&context);
        niri_video_free(v);free(pixels);++cases;
    }
    printf("PASS: AVC444 full-chroma layout %d odd-size cases, %d consecutive main/aux views decoded in one reference chain\n",cases,decoded);
}
