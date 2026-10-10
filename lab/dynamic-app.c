#define _GNU_SOURCE
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include <linux/input-event-codes.h>
#include <sys/mman.h>
#include <unistd.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdbool.h>
#include <string.h>

struct app {
    struct wl_display *display; struct wl_compositor *compositor;
    struct wl_shm *shm; struct wl_seat *seat;
    struct xdg_wm_base *wm; struct wl_surface *surface;
    struct xdg_surface *xdg; struct xdg_toplevel *top;
    unsigned width, height, frames; bool configured, key, button, quit;
};
struct buffer { struct wl_buffer *wl; void *data; size_t size; };
static void release(void *data, struct wl_buffer *wl) {
    struct buffer *b=data; wl_buffer_destroy(wl); munmap(b->data,b->size); free(b);
}
static const struct wl_buffer_listener buffer_listener={.release=release};
static void draw(struct app *a);
static void frame_done(void *data,struct wl_callback *cb,uint32_t time) {
    (void)time; wl_callback_destroy(cb); draw(data);
}
static const struct wl_callback_listener frame_listener={.done=frame_done};
static void draw(struct app *a) {
    if (!a->configured || a->quit) return;
    struct buffer *b=calloc(1,sizeof *b);
    b->size=(size_t)a->width*a->height*4;
    int fd=memfd_create("niri-rdp-test-app",MFD_CLOEXEC);
    if(fd<0 || ftruncate(fd,b->size)<0) {perror("memfd");exit(1);}
    b->data=mmap(NULL,b->size,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);
    if(b->data==MAP_FAILED){perror("mmap");exit(1);}
    // Native little-endian XRGB8888: exact baseline BGRA 180,40,30,255.
    uint32_t color=a->button?0xff1eb428:a->key?0xffb4281e:0xff1e28b4;
    uint32_t *pixels=b->data;
    for(unsigned y=0;y<a->height;y++)
        for(unsigned x=0;x<a->width;x++)
            pixels[(size_t)y*a->width+x]=y+16>=a->height?
                (0xff000000|((a->frames*3+x)%256)<<16|((a->frames*7)%256)<<8|x%256):color;
    struct wl_shm_pool *pool=wl_shm_create_pool(a->shm,fd,b->size);
    b->wl=wl_shm_pool_create_buffer(pool,0,a->width,a->height,a->width*4,WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);close(fd);
    wl_buffer_add_listener(b->wl,&buffer_listener,b);
    struct wl_callback *cb=wl_surface_frame(a->surface);
    wl_callback_add_listener(cb,&frame_listener,a);
    wl_surface_attach(a->surface,b->wl,0,0);
    wl_surface_damage(a->surface,0,0,a->width,a->height);
    wl_surface_commit(a->surface);
    if(++a->frames%60==0) printf("FRAME %u size=%ux%u key=%d button=%d\n",a->frames,a->width,a->height,a->key,a->button);
}
static void ping(void *d,struct xdg_wm_base *wm,uint32_t s){(void)d;xdg_wm_base_pong(wm,s);}
static const struct xdg_wm_base_listener wm_listener={.ping=ping};
static void configured(void *d,struct xdg_surface *s,uint32_t serial) {
    struct app *a=d;xdg_surface_ack_configure(s,serial);
    if(!a->configured){a->configured=true;draw(a);}
}
static const struct xdg_surface_listener xdg_listener={.configure=configured};
static void size(void *d,struct xdg_toplevel *t,int32_t w,int32_t h,struct wl_array *states){
    (void)t;(void)states;struct app *a=d;
    if(w>0&&h>0&&w<=8192&&h<=8192){a->width=w;a->height=h;}
}
static void closed(void *d,struct xdg_toplevel *t){(void)t;((struct app*)d)->quit=true;}
static const struct xdg_toplevel_listener top_listener={.configure=size,.close=closed};
static void keymap(void*d,struct wl_keyboard*k,uint32_t format,int32_t fd,uint32_t size){(void)d;(void)k;(void)format;(void)size;close(fd);}
static void kenter(void*d,struct wl_keyboard*k,uint32_t s,struct wl_surface*f,struct wl_array*keys){(void)d;(void)k;(void)s;(void)f;(void)keys;puts("KEYBOARD_ENTER");}
static void kleave(void*d,struct wl_keyboard*k,uint32_t s,struct wl_surface*f){(void)k;(void)s;(void)f;((struct app*)d)->key=false;puts("KEYBOARD_LEAVE");}
static void key(void*d,struct wl_keyboard*k,uint32_t s,uint32_t time,uint32_t code,uint32_t state){
    (void)k;(void)s;(void)time;struct app*a=d;
    if(code==KEY_A)a->key=state==WL_KEYBOARD_KEY_STATE_PRESSED;
    printf("KEY %u %u\n",code,state);
}
static void mods(void*d,struct wl_keyboard*k,uint32_t s,uint32_t dep,uint32_t lat,uint32_t lock,uint32_t group){(void)d;(void)k;(void)s;(void)dep;(void)lat;(void)lock;(void)group;}
static void repeat(void*d,struct wl_keyboard*k,int32_t rate,int32_t delay){(void)d;(void)k;(void)rate;(void)delay;}
static const struct wl_keyboard_listener keyboard_listener={.keymap=keymap,.enter=kenter,.leave=kleave,.key=key,.modifiers=mods,.repeat_info=repeat};
static void penter(void*d,struct wl_pointer*p,uint32_t s,struct wl_surface*f,wl_fixed_t x,wl_fixed_t y){(void)d;(void)p;(void)s;(void)f;printf("POINTER_ENTER %.1f %.1f\n",wl_fixed_to_double(x),wl_fixed_to_double(y));}
static void pleave(void*d,struct wl_pointer*p,uint32_t s,struct wl_surface*f){(void)p;(void)s;(void)f;((struct app*)d)->button=false;puts("POINTER_LEAVE");}
static void motion(void*d,struct wl_pointer*p,uint32_t time,wl_fixed_t x,wl_fixed_t y){(void)d;(void)p;(void)time;printf("MOTION %.1f %.1f\n",wl_fixed_to_double(x),wl_fixed_to_double(y));}
static void button(void*d,struct wl_pointer*p,uint32_t s,uint32_t time,uint32_t code,uint32_t state){(void)p;(void)s;(void)time;((struct app*)d)->button=state==WL_POINTER_BUTTON_STATE_PRESSED;printf("BUTTON %u %u\n",code,state);}
static void axis(void*d,struct wl_pointer*p,uint32_t t,uint32_t a,wl_fixed_t v){(void)d;(void)p;(void)t;(void)a;(void)v;}
static const struct wl_pointer_listener pointer_listener={.enter=penter,.leave=pleave,.motion=motion,.button=button,.axis=axis};
static void caps(void*d,struct wl_seat*s,uint32_t c){
    if(c&WL_SEAT_CAPABILITY_KEYBOARD)wl_keyboard_add_listener(wl_seat_get_keyboard(s),&keyboard_listener,d);
    if(c&WL_SEAT_CAPABILITY_POINTER)wl_pointer_add_listener(wl_seat_get_pointer(s),&pointer_listener,d);
}
static void seat_name(void*d,struct wl_seat*s,const char*n){(void)d;(void)s;printf("SEAT %s\n",n);}
static const struct wl_seat_listener seat_listener={.capabilities=caps,.name=seat_name};
static void global(void*d,struct wl_registry*r,uint32_t n,const char*i,uint32_t v){
    struct app*a=d;
    if(!strcmp(i,"wl_compositor"))a->compositor=wl_registry_bind(r,n,&wl_compositor_interface,v<4?v:4);
    else if(!strcmp(i,"wl_shm"))a->shm=wl_registry_bind(r,n,&wl_shm_interface,1);
    else if(!strcmp(i,"xdg_wm_base")){a->wm=wl_registry_bind(r,n,&xdg_wm_base_interface,1);xdg_wm_base_add_listener(a->wm,&wm_listener,a);}
    else if(!strcmp(i,"wl_seat")){a->seat=wl_registry_bind(r,n,&wl_seat_interface,4);wl_seat_add_listener(a->seat,&seat_listener,a);}
}
static void removed(void*d,struct wl_registry*r,uint32_t n){(void)d;(void)r;(void)n;}
static const struct wl_registry_listener registry_listener={.global=global,.global_remove=removed};
int main(void){
    setvbuf(stdout,NULL,_IOLBF,0);struct app a={.width=640,.height=480};
    a.display=wl_display_connect(NULL);if(!a.display){perror("Wayland connect");return 1;}
    wl_registry_add_listener(wl_display_get_registry(a.display),&registry_listener,&a);
    if(wl_display_roundtrip(a.display)<0||!a.compositor||!a.shm||!a.wm||!a.seat)return 1;
    a.surface=wl_compositor_create_surface(a.compositor);a.xdg=xdg_wm_base_get_xdg_surface(a.wm,a.surface);
    xdg_surface_add_listener(a.xdg,&xdg_listener,&a);a.top=xdg_surface_get_toplevel(a.xdg);
    xdg_toplevel_add_listener(a.top,&top_listener,&a);xdg_toplevel_set_title(a.top,"Niri RDP input proof");
    xdg_toplevel_set_app_id(a.top,"niri-rdp-proof");wl_surface_commit(a.surface);
    while(!a.quit&&wl_display_dispatch(a.display)>=0){}
    wl_display_disconnect(a.display);return 0;
}
