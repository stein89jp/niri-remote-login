/* Opt-in NVIDIA GBM/EGL/CUDA/NVENC validation. The fixture writes synthetic
 * pixels to GBM and downloads conversion results for comparison; production
 * capture/encoding performs neither CPU operation. */
#define _GNU_SOURCE
#define NIRI_VIDEO_GPU 1
#include <assert.h>
#include <fcntl.h>
#include <errno.h>
#include <math.h>
#include <gbm.h>
#include <sys/stat.h>
#include <unistd.h>
#include "../bridge/src/video.c"
int main(int argc,char **argv) {
    const char *node=argc>1?argv[1]:"/dev/dri/renderD128";
    int fd=open(node,O_RDWR|O_CLOEXEC);if(fd<0){perror(node);return 1;}
    struct stat st;assert(fstat(fd,&st)==0);
    struct gbm_device *device=gbm_create_device(fd);assert(device);
    int widths[]={641,1920},heights[]={369,1080},cases=0;
    av_log_set_level(AV_LOG_ERROR);
    for(int dimension=0;dimension<2;++dimension) {
        int w=widths[dimension],h=heights[dimension];
        struct gbm_bo *bo=gbm_bo_create(device,w,h,GBM_FORMAT_XRGB8888,GBM_BO_USE_RENDERING|GBM_BO_USE_LINEAR);
        if(!bo)bo=gbm_bo_create(device,w,h,GBM_FORMAT_XRGB8888,GBM_BO_USE_RENDERING);
        if(!bo){fprintf(stderr,"GBM allocation unavailable: %s\n",strerror(errno));return 1;}
        uint8_t *pixels=malloc((size_t)w*h*4);assert(pixels);
        for(int y=0;y<h;++y)for(int x=0;x<w;++x) {
            uint8_t *p=pixels+((size_t)y*w+x)*4;
            const uint8_t colors[6][4]={{0,0,255,255},{0,255,0,255},{255,0,0,255},{0,0,0,255},{255,255,255,255},{100,150,200,255}};
            memcpy(p,colors[(x/32+y/32)%6],4);
        }
        int image_fd=gbm_bo_get_fd(bo);assert(image_fd>=0);
        char error[256]={0};
        struct niri_video *gpu=niri_video_new_config(w,h,0,60,12000000,1,0,0,error,sizeof(error));
        struct niri_video *cpu=niri_video_new(w,h,1,60,12000000,error,sizeof(error));
        assert(gpu&&cpu);
        gpu->gpu=video_gpu_new(gpu,st.st_rdev,error,sizeof(error));
        if(!gpu->gpu){fprintf(stderr,"GPU initialization: %s\n",error);return 1;}
        struct niri_gpu *g=gpu->gpu;assert(gpu_current(g));
        uint64_t modifier=gbm_bo_get_modifier(bo);
        EGLint attrs[]={EGL_WIDTH,w,EGL_HEIGHT,h,EGL_LINUX_DRM_FOURCC_EXT,GBM_FORMAT_XRGB8888,
            EGL_DMA_BUF_PLANE0_FD_EXT,image_fd,EGL_DMA_BUF_PLANE0_OFFSET_EXT,0,EGL_DMA_BUF_PLANE0_PITCH_EXT,gbm_bo_get_stride(bo),
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,(EGLint)modifier,EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,(EGLint)(modifier>>32),EGL_NONE};
        EGLImageKHR image=g->create_image(g->display,EGL_NO_CONTEXT,EGL_LINUX_DMA_BUF_EXT,NULL,attrs);
        if(image==EGL_NO_IMAGE_KHR){fprintf(stderr,"fixture image import failed EGL=%#x\n",eglGetError());return 1;}
        glBindTexture(GL_TEXTURE_2D,g->texture);g->target_image(GL_TEXTURE_2D,image);
        GLuint framebuffer;glGenFramebuffers(1,&framebuffer);glBindFramebuffer(GL_FRAMEBUFFER,framebuffer);
        glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,g->texture,0);
        assert(glCheckFramebufferStatus(GL_FRAMEBUFFER)==GL_FRAMEBUFFER_COMPLETE);
        glEnable(GL_SCISSOR_TEST);
        for(int y=0;y<h;y+=32)for(int x=0;x<w;x+=32){
            const uint8_t *p=pixels+((size_t)y*w+x)*4;
            glScissor(x,y,32,32);glClearColor(p[2]/255.0f,p[1]/255.0f,p[0]/255.0f,1);glClear(GL_COLOR_BUFFER_BIT);
        }
        glDisable(GL_SCISSOR_TEST);glFinish();glBindFramebuffer(GL_FRAMEBUFFER,0);glDeleteFramebuffers(1,&framebuffer);
        g->destroy_image(g->display,image);gpu_release(g);
        const uint8_t *main,*aux,*cpu_packet;int main_size,aux_size;
        if(niri_video_encode_dmabuf(gpu,image_fd,GBM_FORMAT_XRGB8888,gbm_bo_get_stride(bo),0,gbm_bo_get_modifier(bo),st.st_rdev,1,&main,&main_size,&aux,&aux_size,error,sizeof(error))<0) {
            fprintf(stderr,"GPU conversion failed: %s\n",error);return 1;
        }
        assert(main_size>0&&aux_size==0);
        assert(!gpu->frame&&!gpu->rgb&&!gpu->full_chroma&&!gpu->auxiliary&&!gpu->scale&&!gpu->tail);
        /* A resumed SHM stream must recreate an IDR encoder, never dereference
           CPU staging released when this encoder adopted GPU surfaces. */
        assert(niri_video_encode(gpu,pixels,w*4,1,&cpu_packet,error,sizeof(error))<0);
        /* Replaced/cancelled update streams can overlap native lifetimes. A
           second encoder on the same cached EGLDisplay must not terminate the
           first encoder's contexts when it is destroyed. */
        struct niri_video *peer=niri_video_new_config(w,h,0,60,12000000,1,0,0,error,sizeof(error));assert(peer);
        peer->gpu=video_gpu_new(peer,st.st_rdev,error,sizeof(error));assert(peer->gpu);
        niri_video_free(peer);
        assert(niri_video_encode_dmabuf(gpu,image_fd,GBM_FORMAT_XRGB8888,gbm_bo_get_stride(bo),0,gbm_bo_get_modifier(bo),st.st_rdev,0,&main,&main_size,&aux,&aux_size,error,sizeof(error))==0);
        assert(niri_video_encode(cpu,pixels,w*4,1,&cpu_packet,error,sizeof(error))>0);
        AVFrame *download=av_frame_alloc();assert(download);
        assert(av_hwframe_transfer_data(download,gpu->gpu->frame,0)==0);
        int max_difference=0,cpu_edge_difference=0;
        for(int plane=0;plane<2;++plane) {
            int ph=plane?gpu->codec->height/2:gpu->codec->height;
            for(int y=0;y<ph;++y)for(int x=0;x<gpu->codec->width;++x) {
                int actual=download->data[plane][y*download->linesize[plane]+x];
                int cpu_expected=plane?cpu->frame->data[1+(x&1)][y*cpu->frame->linesize[1+(x&1)]+x/2]:cpu->frame->data[0][y*cpu->frame->linesize[0]+x];
                int cpu_difference=abs(actual-cpu_expected);if(cpu_difference>cpu_edge_difference)cpu_edge_difference=cpu_difference;
                double expected=0;
                for(int dy=0;dy<(plane?2:1);++dy)for(int dx=0;dx<(plane?2:1);++dx) {
                    int sx=plane?(x/2)*2+dx:x,sy=plane?y*2+dy:y;
                    if(sx>=w)sx=w-1;
                    if(sy>=h)sy=h-1;
                    const uint8_t *p=pixels+((size_t)sy*w+sx)*4;
                    double value=!plane?.2126*p[2]+.7152*p[1]+.0722*p[0]:
                        (x&1)?.5*p[2]-.454153*p[1]-.045847*p[0]+128:
                        -.114572*p[2]-.385428*p[1]+.5*p[0]+128;
                    expected+=fmin(255,fmax(0,value))/(plane?4:1);
                }
                int difference=abs(actual-(int)lround(expected));if(difference>max_difference)max_difference=difference;
            }
        }
        if(max_difference>1){fprintf(stderr,"GPU color/padding differs from full-range709 reference: max=%d\n",max_difference);return 1;}
        assert(niri_video_set_rate(gpu,4000000,15)==1);
        niri_video_free(gpu);
        gpu=niri_video_new_config(w,h,0,15,4000000,1,0,0,error,sizeof(error));assert(gpu);
        assert(niri_video_encode_dmabuf(gpu,image_fd,GBM_FORMAT_XRGB8888,gbm_bo_get_stride(bo),0,gbm_bo_get_modifier(bo),st.st_rdev,0,&main,&main_size,&aux,&aux_size,error,sizeof(error))==0);
        assert(niri_video_encode_dmabuf(gpu,image_fd,GBM_FORMAT_XRGB8888,gbm_bo_get_stride(bo),0,gbm_bo_get_modifier(bo),st.st_rdev+1,0,&main,&main_size,&aux,&aux_size,error,sizeof(error))<0);
        assert(niri_video_encode_dmabuf(gpu,-1,GBM_FORMAT_XRGB8888,gbm_bo_get_stride(bo),0,0,st.st_rdev,0,&main,&main_size,&aux,&aux_size,error,sizeof(error))<0);
        printf("GPU %dx%d full-range709/padding max_difference=%d, CPU resampling edge difference=%d; changed FPS/bitrate and rejected wrong device/fd\n",w,h,max_difference,cpu_edge_difference);
        av_frame_free(&download);niri_video_free(gpu);niri_video_free(cpu);close(image_fd);gbm_bo_destroy(bo);free(pixels);++cases;
    }
    gbm_device_destroy(device);close(fd);printf("PASS: %d GPU-resident conversion cases\n",cases);return 0;
}
