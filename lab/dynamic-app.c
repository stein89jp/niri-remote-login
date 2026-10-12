#define _GNU_SOURCE
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include "text-input-client-protocol.h"
#include <linux/input-event-codes.h>
#include <sys/mman.h>
#include <unistd.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdbool.h>
#include <string.h>
#include <xkbcommon/xkbcommon.h>

struct app {
    struct wl_display *display; struct wl_compositor *compositor;
    struct wl_shm *shm; struct wl_seat *seat;
    struct xdg_wm_base *wm; struct wl_surface *surface;
    struct xdg_surface *xdg; struct xdg_toplevel *top;
    unsigned width, height, frames; bool configured, key, button, quit;
    struct xkb_context *xkb; struct xkb_state *xkb_state;
    char typed[128]; size_t typed_length;
    struct zwp_text_input_manager_v3 *text_manager;
    struct zwp_text_input_v3 *text_input;
    char *pending_commit;
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
static void keymap(void*d,struct wl_keyboard*k,uint32_t format,int32_t fd,uint32_t size){
    (void)k;struct app*a=d;
    if(format!=WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1 || !size){close(fd);return;}
    char *text=mmap(NULL,size,PROT_READ,MAP_PRIVATE,fd,0);close(fd);
    if(text==MAP_FAILED){perror("keyboard keymap mmap");exit(1);}
    if(!a->xkb)a->xkb=xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    struct xkb_keymap *map=xkb_keymap_new_from_string(a->xkb,text,XKB_KEYMAP_FORMAT_TEXT_V1,XKB_KEYMAP_COMPILE_NO_FLAGS);
    munmap(text,size);
    if(!map){fputs("invalid keyboard XKB map\n",stderr);exit(1);}
    xkb_state_unref(a->xkb_state);a->xkb_state=xkb_state_new(map);xkb_keymap_unref(map);
    puts("XKB_KEYMAP");
}
static void kenter(void*d,struct wl_keyboard*k,uint32_t s,struct wl_surface*f,struct wl_array*keys){(void)d;(void)k;(void)s;(void)f;(void)keys;puts("KEYBOARD_ENTER");}
static void kleave(void*d,struct wl_keyboard*k,uint32_t s,struct wl_surface*f){(void)k;(void)s;(void)f;((struct app*)d)->key=false;puts("KEYBOARD_LEAVE");}
static void key(void*d,struct wl_keyboard*k,uint32_t s,uint32_t time,uint32_t code,uint32_t state){
    (void)k;(void)s;(void)time;struct app*a=d;
    if(code==KEY_A)a->key=state==WL_KEYBOARD_KEY_STATE_PRESSED;
    printf("KEY %u %u\n",code,state);
    if(state==WL_KEYBOARD_KEY_STATE_PRESSED && a->xkb_state){
        xkb_keysym_t sym=xkb_state_key_get_one_sym(a->xkb_state,code+8);
        printf("KEYSYM %u\n",sym);
        if(sym==XKB_KEY_Escape)a->typed_length=0;
        else if(sym==XKB_KEY_BackSpace){
            if(a->typed_length){
                do { a->typed_length--; }
                while(a->typed_length && ((unsigned char)a->typed[a->typed_length]&0xc0)==0x80);
            }
        }
        else if(sym==XKB_KEY_Return){
            a->typed[a->typed_length]='\0';printf("XKB_TEXT %s\n",a->typed);a->typed_length=0;
        }else{
            char text[16];int len=xkb_state_key_get_utf8(a->xkb_state,code+8,text,sizeof text);
            if(len>0 && (size_t)len<sizeof text && a->typed_length+(size_t)len<sizeof a->typed){
                memcpy(a->typed+a->typed_length,text,(size_t)len);a->typed_length+=(size_t)len;
            }
        }
    }
}
static void mods(void*d,struct wl_keyboard*k,uint32_t s,uint32_t dep,uint32_t lat,uint32_t lock,uint32_t group){
    (void)k;(void)s;struct app*a=d;
    if(a->xkb_state)xkb_state_update_mask(a->xkb_state,dep,lat,lock,0,0,group);
}
static void repeat(void*d,struct wl_keyboard*k,int32_t rate,int32_t delay){(void)d;(void)k;(void)rate;(void)delay;}
static const struct wl_keyboard_listener keyboard_listener={.keymap=keymap,.enter=kenter,.leave=kleave,.key=key,.modifiers=mods,.repeat_info=repeat};
static void text_enter(void *data, struct zwp_text_input_v3 *input, struct wl_surface *surface) {
    (void)data; (void)surface;
    zwp_text_input_v3_enable(input);
    zwp_text_input_v3_set_content_type(input, ZWP_TEXT_INPUT_V3_CONTENT_HINT_NONE,
                                     ZWP_TEXT_INPUT_V3_CONTENT_PURPOSE_NORMAL);
    zwp_text_input_v3_set_cursor_rectangle(input, 40, 40, 1, 20);
    zwp_text_input_v3_commit(input);
    puts("TEXT_INPUT_ENTER");
}
static void text_leave(void *data, struct zwp_text_input_v3 *input, struct wl_surface *surface) {
    (void)data; (void)surface;
    zwp_text_input_v3_disable(input); zwp_text_input_v3_commit(input);
}
static void text_preedit(void *data, struct zwp_text_input_v3 *input, const char *text,
                         int32_t begin, int32_t end) {
    (void)data; (void)input; (void)begin; (void)end;
    if (text && *text) printf("IME_PREEDIT %s\n", text);
}
static void text_commit(void *data, struct zwp_text_input_v3 *input, const char *text) {
    (void)input; struct app *a=data;
    free(a->pending_commit); a->pending_commit=text ? strdup(text) : NULL;
}
static void text_delete(void *data, struct zwp_text_input_v3 *input, uint32_t before, uint32_t after) {
    (void)data; (void)input; (void)before; (void)after;
}
static void text_done(void *data, struct zwp_text_input_v3 *input, uint32_t serial) {
    (void)input; (void)serial; struct app *a=data;
    if (a->pending_commit) {
        printf("IME_COMMIT %s\n", a->pending_commit);
        free(a->pending_commit); a->pending_commit=NULL;
    }
}
static const struct zwp_text_input_v3_listener text_listener={
    .enter=text_enter,.leave=text_leave,.preedit_string=text_preedit,.commit_string=text_commit,
    .delete_surrounding_text=text_delete,.done=text_done
};
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
    else if(!strcmp(i,"zwp_text_input_manager_v3") && getenv("NIRI_RDP_TEST_IME"))
        a->text_manager=wl_registry_bind(r,n,&zwp_text_input_manager_v3_interface,1);
}
static void removed(void*d,struct wl_registry*r,uint32_t n){(void)d;(void)r;(void)n;}
static const struct wl_registry_listener registry_listener={.global=global,.global_remove=removed};
int main(void){
    setvbuf(stdout,NULL,_IOLBF,0);struct app a={.width=640,.height=480};
    a.display=wl_display_connect(NULL);if(!a.display){perror("Wayland connect");return 1;}
    wl_registry_add_listener(wl_display_get_registry(a.display),&registry_listener,&a);
    if(wl_display_roundtrip(a.display)<0||!a.compositor||!a.shm||!a.wm||!a.seat)return 1;
    if(getenv("NIRI_RDP_TEST_IME")) {
        if(!a.text_manager)return 1;
        a.text_input=zwp_text_input_manager_v3_get_text_input(a.text_manager,a.seat);
        zwp_text_input_v3_add_listener(a.text_input,&text_listener,&a);
    }
    a.surface=wl_compositor_create_surface(a.compositor);a.xdg=xdg_wm_base_get_xdg_surface(a.wm,a.surface);
    xdg_surface_add_listener(a.xdg,&xdg_listener,&a);a.top=xdg_surface_get_toplevel(a.xdg);
    xdg_toplevel_add_listener(a.top,&top_listener,&a);xdg_toplevel_set_title(a.top,"Niri RDP input proof");
    xdg_toplevel_set_app_id(a.top,"niri-rdp-proof");wl_surface_commit(a.surface);
    while(!a.quit&&wl_display_dispatch(a.display)>=0){}
    free(a.pending_commit);wl_display_disconnect(a.display);return 0;
}
