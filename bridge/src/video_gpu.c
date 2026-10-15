/* GPU-resident Linux DMA-BUF -> full-range BT709 -> NVENC path.
 * EGL/GL and the CUDA driver are optional runtime capabilities. No CUDA toolkit
 * or CPU readback is used. One reusable GL buffer and two FFmpeg CUDA surfaces
 * bound work; a device-to-device copy bridges GL storage and NVENC surfaces.
 * Each capture slot's EGLImage and texture are imported once and cached by
 * DMA-BUF identity and layout; resize recreates the encoder and its cache.
 * DMA-BUF implicit fences are honored by EGL import; GL completion precedes CUDA
 * mapping, and CUDA completion precedes encoding and caller buffer release. */
#ifdef NIRI_VIDEO_GPU
#include <EGL/egl.h>
#include <EGL/eglext.h>
#define GL_GLEXT_PROTOTYPES 1
#include <GL/gl.h>
#include <GL/glext.h>
#include <libavutil/hwcontext.h>
#include <dlfcn.h>
#include <sys/stat.h>
#include <unistd.h>
#include <pthread.h>

/* EGL caches one display per device/attribute tuple across encoder instances.
 * Cancellation/reactivation can leave an old blocking task alive while its
 * replacement creates a context. Only the last owned display reference may
 * terminate it; contexts/surfaces and CUDA resources remain per encoder. */
static pthread_mutex_t gpu_display_lock=PTHREAD_MUTEX_INITIALIZER;
static struct {EGLDisplay display;unsigned int references;} gpu_displays[16];
static int gpu_display_acquire(EGLDisplay display) {
    pthread_mutex_lock(&gpu_display_lock);
    for(int i=0;i<16;++i)if(gpu_displays[i].references&&gpu_displays[i].display==display){++gpu_displays[i].references;pthread_mutex_unlock(&gpu_display_lock);return 1;}
    for(int i=0;i<16;++i)if(!gpu_displays[i].references){
        if(!eglInitialize(display,NULL,NULL)){pthread_mutex_unlock(&gpu_display_lock);return 0;}
        gpu_displays[i].display=display;gpu_displays[i].references=1;pthread_mutex_unlock(&gpu_display_lock);return 1;
    }
    pthread_mutex_unlock(&gpu_display_lock);return 0;
}
static void gpu_display_release(EGLDisplay display) {
    pthread_mutex_lock(&gpu_display_lock);
    for(int i=0;i<16;++i)if(gpu_displays[i].references&&gpu_displays[i].display==display){
        if(--gpu_displays[i].references==0){eglTerminate(display);gpu_displays[i].display=EGL_NO_DISPLAY;}
        break;
    }
    pthread_mutex_unlock(&gpu_display_lock);
}

/* The stable public CUDA Driver ABI subset used here. Loading it at runtime
 * retains SHM fallback on machines without an NVIDIA driver or interop support. */
typedef int VideoCuResult;
typedef int VideoCuDevice;
typedef void *VideoCuContext;
typedef void *VideoCuStream;
typedef void *VideoCuResource;
typedef unsigned long long VideoCuDevicePtr;
typedef struct {
    size_t srcXInBytes, srcY; unsigned int srcMemoryType;
    const void *srcHost; VideoCuDevicePtr srcDevice; void *srcArray; size_t srcPitch;
    size_t dstXInBytes, dstY; unsigned int dstMemoryType;
    void *dstHost; VideoCuDevicePtr dstDevice; void *dstArray; size_t dstPitch;
    size_t WidthInBytes, Height;
} VideoCuCopy2D;
/* AVHWDeviceContext's CUDA public prefix, matching libavutil/hwcontext_cuda.h. */
struct VideoAvCuda { VideoCuContext cuda_ctx; VideoCuStream stream; };
/* The capture pool has three GBM slots; one spare entry absorbs a pool
   replacement without evicting a live slot. A cached image holds its DMA-BUF,
   whose inode number the kernel never reuses while it lives. */
#define GPU_IMAGE_CACHE 4
struct gpu_image {
    EGLImageKHR image; GLuint texture;
    dev_t device; ino_t inode; uint32_t fourcc, stride, offset; uint64_t modifier;
    uint64_t used;
};
struct niri_gpu {
    void *cuda_library;
    EGLDisplay display;
    EGLContext context;
    EGLSurface surface;
    int display_acquired;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC target_image;
    /* Conversion samples the cached per-slot textures; this unbound name
       remains for the opt-in fixture that renders synthetic pixels. */
    GLuint texture, buffer, program;
    GLint logical_size, coded_size, view;
    struct gpu_image images[GPU_IMAGE_CACHE];
    uint64_t image_clock;
    VideoCuResource resource;
    AVBufferRef *device;
    AVBufferRef *frames;
    AVFrame *frame;
    VideoCuContext cuda_context;
    VideoCuStream stream;
    VideoCuResult (*push)(VideoCuContext);
    VideoCuResult (*pop)(VideoCuContext *);
    VideoCuResult (*register_buffer)(VideoCuResource *, unsigned int, unsigned int);
    VideoCuResult (*unregister)(VideoCuResource);
    VideoCuResult (*map)(unsigned int, VideoCuResource *, VideoCuStream);
    VideoCuResult (*unmap)(unsigned int, VideoCuResource *, VideoCuStream);
    VideoCuResult (*mapped_pointer)(VideoCuDevicePtr *, size_t *, VideoCuResource);
    VideoCuResult (*copy)(const VideoCuCopy2D *, VideoCuStream);
    VideoCuResult (*synchronize)(VideoCuStream);
    uint64_t device_id;
};

static int gpu_current(struct niri_gpu *g) {
    return eglBindAPI(EGL_OPENGL_API) && eglMakeCurrent(g->display, g->surface, g->surface, g->context) && g->push(g->cuda_context) == 0;
}
static void gpu_release(struct niri_gpu *g) {
    VideoCuContext context;
    g->pop(&context);
    eglMakeCurrent(g->display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
}
/* Texture deletion needs the current context; image destruction only the display. */
static void gpu_image_clear(struct niri_gpu *g, struct gpu_image *entry, int current) {
    if (current && entry->texture) glDeleteTextures(1, &entry->texture);
    if (entry->image != EGL_NO_IMAGE_KHR) g->destroy_image(g->display, entry->image);
    memset(entry, 0, sizeof(*entry));
    entry->image = EGL_NO_IMAGE_KHR;
}
static void video_gpu_free(struct niri_gpu *g) {
    if (!g) return;
    if (g->display != EGL_NO_DISPLAY && g->context != EGL_NO_CONTEXT) {
        eglBindAPI(EGL_OPENGL_API);
        int current = eglMakeCurrent(g->display, g->surface, g->surface, g->context);
        VideoCuContext previous;
        int pushed = g->push && g->cuda_context && g->push(g->cuda_context) == 0;
        if (pushed && g->resource) g->unregister(g->resource);
        for (int i = 0; i < GPU_IMAGE_CACHE; ++i) gpu_image_clear(g, &g->images[i], current);
        if (current) {
            if (g->buffer) glDeleteBuffers(1, &g->buffer);
            if (g->texture) glDeleteTextures(1, &g->texture);
            if (g->program) glDeleteProgram(g->program);
        }
        if (pushed) g->pop(&previous);
        eglMakeCurrent(g->display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        eglDestroyContext(g->display, g->context);
        if (g->surface != EGL_NO_SURFACE) eglDestroySurface(g->display, g->surface);
    }
    if(g->display_acquired)gpu_display_release(g->display);
    av_frame_free(&g->frame);
    av_buffer_unref(&g->frames);
    av_buffer_unref(&g->device);
    if (g->cuda_library) dlclose(g->cuda_library);
    free(g);
}

static const char gpu_conversion_shader[] =
    "#version 430\n"
    "layout(local_size_x=256) in;\n"
    "layout(binding=0) uniform sampler2D desktop;\n"
    "layout(std430,binding=1) buffer Output { uint bytes[]; };\n"
    "uniform ivec2 logical_size; uniform ivec2 coded_size; uniform int view;\n"
    "vec3 yuv(int x,int y) { vec3 c=texelFetch(desktop,clamp(ivec2(x,y),ivec2(0),logical_size-1),0).rgb*255.0;"
    "return clamp(vec3(dot(c,vec3(.2126,.7152,.0722)),dot(c,vec3(-.114572,-.385428,.5))+128.0,dot(c,vec3(.5,-.454153,-.045847))+128.0),0.0,255.0); }\n"
    "uint sample_byte(int at) { int w=coded_size.x; int h=coded_size.y; int y=at/w; int x=at%w;"
    "if(y<h) { if(view==0) return uint(round(yuv(x,y).x));"
    "int sy=(y/16)*16+(y%8)*2+1; return uint(round(yuv(x,sy)[y%16<8?1:2])); }"
    "int sy=(y-h)*2; int sx=(x/2)*2; int p=1+(x%2);"
    "if(view==1) return uint(round(yuv(sx+1,sy)[p]));"
    "return uint(round((yuv(sx,sy)[p]+yuv(sx+1,sy)[p]+yuv(sx,sy+1)[p]+yuv(sx+1,sy+1)[p])*.25)); }\n"
    "void main() { uint i=gl_GlobalInvocationID.x; int n=coded_size.x*coded_size.y*3/2; if(i>=uint(n/4)) return;"
    "int at=int(i)*4; bytes[i+uint(view*n/4)]=sample_byte(at)|(sample_byte(at+1)<<8)|(sample_byte(at+2)<<16)|(sample_byte(at+3)<<24); }\n";

static struct niri_gpu *video_gpu_new(struct niri_video *v, uint64_t device_id,
                                      char *error, size_t length) {
    if (length) error[0] = '\0';
    if (!v->codec) { snprintf(error, length, "H.264 encoder context released by failed GPU promotion"); return NULL; }
    struct niri_gpu *g = calloc(1, sizeof(*g));
    if (!g) { snprintf(error, length, "allocate DMA-BUF GPU state"); return NULL; }
    g->display = EGL_NO_DISPLAY; g->context = EGL_NO_CONTEXT; g->surface = EGL_NO_SURFACE;
    g->device_id = device_id;
#define GPU_FAIL(...) do { snprintf(error, length, __VA_ARGS__); goto fail; } while (0)
    g->cuda_library = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!g->cuda_library) GPU_FAIL("CUDA driver unavailable for DMA-BUF import");
#define GPU_CUDA(member, name) do { *(void **)(&g->member) = dlsym(g->cuda_library, name); if (!g->member) GPU_FAIL("CUDA driver lacks %s", name); } while (0)
    GPU_CUDA(push, "cuCtxPushCurrent_v2"); GPU_CUDA(pop, "cuCtxPopCurrent_v2");
    GPU_CUDA(register_buffer, "cuGraphicsGLRegisterBuffer"); GPU_CUDA(unregister, "cuGraphicsUnregisterResource");
    GPU_CUDA(map, "cuGraphicsMapResources"); GPU_CUDA(unmap, "cuGraphicsUnmapResources");
    GPU_CUDA(mapped_pointer, "cuGraphicsResourceGetMappedPointer_v2");
    GPU_CUDA(copy, "cuMemcpy2DAsync_v2"); GPU_CUDA(synchronize, "cuStreamSynchronize");
#undef GPU_CUDA
    PFNEGLQUERYDEVICESEXTPROC query_devices = (void *)eglGetProcAddress("eglQueryDevicesEXT");
    PFNEGLQUERYDEVICESTRINGEXTPROC query_string = (void *)eglGetProcAddress("eglQueryDeviceStringEXT");
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform_display = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    g->create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
    g->destroy_image = (void *)eglGetProcAddress("eglDestroyImageKHR");
    g->target_image = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!query_devices || !query_string || !platform_display || !g->create_image || !g->destroy_image || !g->target_image)
        GPU_FAIL("EGL lacks device, platform display or DMA-BUF image extensions");
    EGLDeviceEXT devices[16]; EGLint count = 0;
    if (!query_devices(16, devices, &count)) GPU_FAIL("EGL device query failed (%#x)", eglGetError());
    for (int i = 0; i < count; ++i) {
        const char *node = query_string(devices[i], EGL_DRM_RENDER_NODE_FILE_EXT);
        struct stat st;
        if (node && stat(node, &st) == 0 && (uint64_t)st.st_rdev == device_id) {
            g->display = platform_display(EGL_PLATFORM_DEVICE_EXT, devices[i], NULL); break;
        }
    }
    if(g->display==EGL_NO_DISPLAY)GPU_FAIL("no EGL device matches the DMA-BUF render node");
    if(!gpu_display_acquire(g->display))GPU_FAIL("EGL display initialization failed (%#x)",eglGetError());
    g->display_acquired=1;
    if(!eglBindAPI(EGL_OPENGL_API))GPU_FAIL("EGL OpenGL API unavailable");
    const EGLint attributes[] = {EGL_SURFACE_TYPE,EGL_PBUFFER_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_BIT,EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,EGL_NONE};
    EGLConfig config; EGLint configs;
    if (!eglChooseConfig(g->display, attributes, &config, 1, &configs) || configs != 1) GPU_FAIL("no EGL pbuffer/OpenGL configuration");
    const EGLint context_attributes[] = {EGL_CONTEXT_MAJOR_VERSION,4,EGL_CONTEXT_MINOR_VERSION,3,EGL_CONTEXT_OPENGL_PROFILE_MASK,EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,EGL_NONE};
    g->context = eglCreateContext(g->display, config, EGL_NO_CONTEXT, context_attributes);
    const EGLint surface_attributes[] = {EGL_WIDTH,1,EGL_HEIGHT,1,EGL_NONE};
    g->surface = eglCreatePbufferSurface(g->display, config, surface_attributes);
    if (g->context == EGL_NO_CONTEXT || g->surface == EGL_NO_SURFACE || !eglMakeCurrent(g->display, g->surface, g->surface, g->context))
        GPU_FAIL("create OpenGL 4.3 context failed (%#x)", eglGetError());
    VideoCuResult (*initialize)(unsigned int) = dlsym(g->cuda_library, "cuInit");
    VideoCuResult (*gl_devices)(unsigned int *, VideoCuDevice *, unsigned int, unsigned int) = dlsym(g->cuda_library, "cuGLGetDevices_v2");
    if (!gl_devices) gl_devices = dlsym(g->cuda_library, "cuGLGetDevices");
    VideoCuDevice cuda_devices[16]; unsigned int cuda_count = 0;
    if (!initialize || !gl_devices || initialize(0) != 0 || gl_devices(&cuda_count, cuda_devices, 16, 1) != 0 || cuda_count != 1)
        GPU_FAIL("CUDA cannot identify exactly one device for the OpenGL context");
    char ordinal[16]; snprintf(ordinal, sizeof(ordinal), "%d", cuda_devices[0]);
    int result = av_hwdevice_ctx_create(&g->device, AV_HWDEVICE_TYPE_CUDA, ordinal, NULL, 0);
    if (result < 0) { error_text(error, length, "create FFmpeg CUDA device", result); goto fail; }
    AVHWDeviceContext *device = (void *)g->device->data;
    struct VideoAvCuda *cuda = device->hwctx;
    g->cuda_context = cuda->cuda_ctx; g->stream = cuda->stream;
    g->frames = av_hwframe_ctx_alloc(g->device);
    if (!g->frames) GPU_FAIL("allocate CUDA frame pool");
    AVHWFramesContext *frames = (void *)g->frames->data;
    frames->format = AV_PIX_FMT_CUDA; frames->sw_format = AV_PIX_FMT_NV12;
    frames->width = v->codec->width; frames->height = v->codec->height; frames->initial_pool_size = 2;
    result = av_hwframe_ctx_init(g->frames);
    if (result < 0) { error_text(error, length, "initialize CUDA frame pool", result); goto fail; }
    g->frame = av_frame_alloc();
    if (!g->frame || av_hwframe_get_buffer(g->frames, g->frame, 0) < 0) GPU_FAIL("allocate CUDA encoder surface");
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    const GLchar *source = gpu_conversion_shader;
    glShaderSource(shader, 1, &source, NULL); glCompileShader(shader);
    GLint okay; glGetShaderiv(shader, GL_COMPILE_STATUS, &okay);
    if (!okay) { glGetShaderInfoLog(shader, length, NULL, error); glDeleteShader(shader); if (!error[0]) GPU_FAIL("compile GPU conversion shader"); goto fail; }
    g->program = glCreateProgram(); glAttachShader(g->program, shader); glLinkProgram(g->program); glDeleteShader(shader);
    glGetProgramiv(g->program, GL_LINK_STATUS, &okay);
    if (!okay) { glGetProgramInfoLog(g->program, length, NULL, error); if (!error[0]) GPU_FAIL("link GPU conversion shader"); goto fail; }
    g->logical_size = glGetUniformLocation(g->program, "logical_size");
    g->coded_size = glGetUniformLocation(g->program, "coded_size");
    g->view = glGetUniformLocation(g->program, "view");
    glGenTextures(1, &g->texture);
    glGenBuffers(1, &g->buffer); glBindBuffer(GL_SHADER_STORAGE_BUFFER, g->buffer);
    size_t bytes = (size_t)v->codec->width * v->codec->height * 3 / 2 * (v->avc444 ? 2 : 1);
    glBufferData(GL_SHADER_STORAGE_BUFFER, bytes, NULL, GL_DYNAMIC_DRAW);
    if (g->push(g->cuda_context) != 0) GPU_FAIL("CUDA context activation failed");
    int registered = g->register_buffer(&g->resource, g->buffer, 1) == 0; /* READ_ONLY */
    VideoCuContext prior; g->pop(&prior);
    if (!registered) GPU_FAIL("CUDA cannot register the OpenGL conversion buffer");
    GLenum gl_error = glGetError();
    if (gl_error != GL_NO_ERROR) GPU_FAIL("OpenGL conversion setup failed (%#x)", gl_error);
    /* Replace the SHM encoder only after import/conversion resources exist.
       Release it before opening the GPU-surface context: NVENC sessions are a
       bounded driver resource and must never overlap here. If the open fails,
       v has no codec and the caller discards it, resuming from a fresh SHM IDR. */
    int bitrate = (int)v->codec->bit_rate;
    const AVCodec *codec = avcodec_find_encoder_by_name("h264_nvenc");
    if (!codec) GPU_FAIL("FFmpeg lacks h264_nvenc");
    AVCodecContext *hardware = avcodec_alloc_context3(codec);
    if (!hardware) GPU_FAIL("allocate GPU-surface NVENC context");
    avcodec_free_context(&v->codec);
    hardware->pix_fmt = AV_PIX_FMT_CUDA; hardware->hw_frames_ctx = av_buffer_ref(g->frames);
    AVDictionary *options = NULL;
    nvenc_configure(v, hardware, bitrate, &options);
    result = avcodec_open2(hardware, codec, &options); av_dict_free(&options);
    if (result < 0) { avcodec_free_context(&hardware); error_text(error,length,"open GPU-surface NVENC",result); goto fail; }
    v->codec = hardware;
    /* A GPU->SHM transition recreates the encoder and starts with an IDR.
       Keeping full RGB/444 CPU staging allocations after promotion would
       waste several whole desktop images for the lifetime of the session. */
    sws_freeContext(v->scale);v->scale=NULL;
    av_frame_free(&v->frame);av_frame_free(&v->rgb);
    av_frame_free(&v->full_chroma);av_frame_free(&v->auxiliary);
    av_freep(&v->tail);
    eglMakeCurrent(g->display,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);
    return g;
fail:
#undef GPU_FAIL
    if (length && !error[0]) snprintf(error,length,"DMA-BUF GPU capability/import initialization failed");
    video_gpu_free(g); return NULL;
}

/* Return the cached texture for this DMA-BUF and layout, importing it into the
   empty or least recently used entry on a miss. The context must be current. */
static struct gpu_image *gpu_image_for(struct niri_gpu *g, struct niri_video *v, int fd,
                                       uint32_t fourcc, uint32_t stride, uint32_t offset,
                                       uint64_t modifier, char *error, size_t length) {
    struct stat st;
    if (fstat(fd, &st) != 0) { snprintf(error, length, "DMA-BUF identity unavailable"); return NULL; }
    struct gpu_image *entry = NULL;
    for (int i = 0; i < GPU_IMAGE_CACHE; ++i) {
        struct gpu_image *candidate = &g->images[i];
        if (candidate->image != EGL_NO_IMAGE_KHR && candidate->device == st.st_dev &&
            candidate->inode == st.st_ino && candidate->fourcc == fourcc && candidate->stride == stride &&
            candidate->offset == offset && candidate->modifier == modifier) {
            candidate->used = ++g->image_clock;
            return candidate;
        }
        if (!entry || (entry->image != EGL_NO_IMAGE_KHR &&
                       (candidate->image == EGL_NO_IMAGE_KHR || candidate->used < entry->used)))
            entry = candidate;
    }
    gpu_image_clear(g, entry, 1);
    EGLint attributes[] = {EGL_WIDTH,v->width,EGL_HEIGHT,v->height,EGL_LINUX_DRM_FOURCC_EXT,(EGLint)fourcc,
        EGL_DMA_BUF_PLANE0_FD_EXT,fd,EGL_DMA_BUF_PLANE0_OFFSET_EXT,(EGLint)offset,EGL_DMA_BUF_PLANE0_PITCH_EXT,(EGLint)stride,
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,(EGLint)modifier,EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,(EGLint)(modifier>>32),EGL_NONE};
    entry->image = g->create_image(g->display,EGL_NO_CONTEXT,EGL_LINUX_DMA_BUF_EXT,NULL,attributes);
    if (entry->image == EGL_NO_IMAGE_KHR) { snprintf(error,length,"unsupported DMA-BUF modifier/import (%#x)",eglGetError()); return NULL; }
    glGenTextures(1, &entry->texture); glBindTexture(GL_TEXTURE_2D, entry->texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    g->target_image(GL_TEXTURE_2D, entry->image);
    entry->device = st.st_dev; entry->inode = st.st_ino; entry->fourcc = fourcc; entry->stride = stride;
    entry->offset = offset; entry->modifier = modifier; entry->used = ++g->image_clock;
    return entry;
}

int niri_video_encode_dmabuf(struct niri_video *v, int fd, uint32_t fourcc, uint32_t stride,
                             uint32_t offset, uint64_t modifier, uint64_t device_id,
                             int force_idr, const uint8_t **main_data, int *main_size,
                             const uint8_t **aux_data, int *aux_size, char *error, size_t length) {
    const uint32_t XRGB8888 = 0x34325258, ARGB8888 = 0x34325241;
    if (!v || fd < 0 || (fourcc != XRGB8888 && fourcc != ARGB8888) || stride < (uint32_t)v->width*4) {
        snprintf(error,length,"unsupported DMA-BUF input layout");return -1;
    }
    if (!v->gpu) { v->gpu = video_gpu_new(v,device_id,error,length); if (!v->gpu) return -1; force_idr = 1; }
    struct niri_gpu *g = v->gpu;
    if (g->device_id != device_id || !gpu_current(g)) { snprintf(error,length,"DMA-BUF device/context mismatch");return -1; }
    double start = video_now_ms(); v->encode_ms = 0;
    glActiveTexture(GL_TEXTURE0);
    struct gpu_image *image = gpu_image_for(g,v,fd,fourcc,stride,offset,modifier,error,length);
    if (!image) { gpu_release(g);return -1; }
    glBindTexture(GL_TEXTURE_2D,image->texture);
    glUseProgram(g->program);glUniform2i(g->logical_size,v->width,v->height);
    glUniform2i(g->coded_size,v->codec->width,v->codec->height);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER,1,g->buffer);
    size_t view_bytes = (size_t)v->codec->width*v->codec->height*3/2;
    for (int view=0;view<(v->avc444?2:1);++view) {
        glUniform1i(g->view,view);glDispatchCompute((view_bytes/4+255)/256,1,1);
    }
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT);glFinish();
    if (glGetError()!=GL_NO_ERROR) { snprintf(error,length,"GPU color conversion failed");goto fail; }
    VideoCuDevicePtr pointer;size_t size;
    if (g->map(1,&g->resource,g->stream)!=0) { snprintf(error,length,"CUDA GL buffer map failed");goto fail; }
    int mapped=1;
    if (g->mapped_pointer(&pointer,&size,g->resource)!=0 || size<view_bytes*(v->avc444?2:1)) { snprintf(error,length,"CUDA GL buffer storage invalid");goto mapped_fail; }
    *aux_size=0;*aux_data=NULL;
    for (int view=0;view<(v->avc444?2:1);++view) {
        /* AVCodec may retain the previous submitted surface. Let the bounded
           hardware pool supply a writable surface rather than overwriting it. */
        av_frame_unref(g->frame);
        if (av_hwframe_get_buffer(g->frames,g->frame,0)<0) { snprintf(error,length,"GPU encoder surface unavailable");goto mapped_fail; }
        VideoCuCopy2D copy = {0};copy.srcMemoryType=2;copy.dstMemoryType=2;
        copy.srcDevice=pointer+view*view_bytes;copy.srcPitch=v->codec->width;
        copy.dstDevice=(uintptr_t)g->frame->data[0];copy.dstPitch=g->frame->linesize[0];
        copy.WidthInBytes=v->codec->width;copy.Height=v->codec->height;
        if (g->copy(&copy,g->stream)!=0) goto copy_fail;
        copy.srcDevice+= (size_t)v->codec->width*v->codec->height;copy.dstDevice=(uintptr_t)g->frame->data[1];
        copy.dstPitch=g->frame->linesize[1];copy.Height=v->codec->height/2;
        if (g->copy(&copy,g->stream)!=0 || g->synchronize(g->stream)!=0) goto copy_fail;
        v->convert_ms=video_now_ms()-start-v->encode_ms;
        const uint8_t *data;
        int bytes=encode_view(v,g->frame,force_idr && view==0,&data,error,length);
        if (bytes<0) goto mapped_fail;
        if (view==0) {
            *main_size=bytes;
            if (v->avc444) { av_packet_unref(v->main_packet);if(av_packet_ref(v->main_packet,v->packet)<0) goto mapped_fail;*main_data=v->main_packet->data; }
            else *main_data=data;
        } else { *aux_data=data;*aux_size=bytes; }
    }
    g->unmap(1,&g->resource,g->stream);g->synchronize(g->stream);
    gpu_release(g);return 0;
copy_fail:
    snprintf(error,length,"GPU conversion surface copy/synchronization failed");
mapped_fail:
    if (mapped) { g->unmap(1,&g->resource,g->stream);g->synchronize(g->stream); }
fail:
    /* Never reuse an import that took part in a failed conversion. */
    gpu_image_clear(g,image,1);gpu_release(g);return -1;
}
#else
static void video_gpu_free(struct niri_gpu *gpu) { (void)gpu; }
int niri_video_encode_dmabuf(struct niri_video *v, int fd, uint32_t fourcc, uint32_t stride,
                             uint32_t offset, uint64_t modifier, uint64_t device_id,
                             int force_idr, const uint8_t **main_data, int *main_size,
                             const uint8_t **aux_data, int *aux_size, char *error, size_t length) {
    (void)v;(void)fd;(void)fourcc;(void)stride;(void)offset;(void)modifier;(void)device_id;
    (void)force_idr;(void)main_data;(void)main_size;(void)aux_data;(void)aux_size;
    snprintf(error,length,"DMA-BUF GPU support unavailable in this build");return -1;
}
#endif
