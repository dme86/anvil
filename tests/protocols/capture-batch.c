#define _GNU_SOURCE
/* Real-client regression: parallel capture protocols must share work, not pixels between outputs. */
#include <wayland-client.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include "layer-shell-client.h"
#include "image-source-client.h"
#include "image-copy-client.h"
#include "screencopy-client.h"

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL capture-batch line %d: %s\n", __LINE__, #c); exit(1); } } while (0)
#define MAX_OUTPUTS 16
#define FRAMES_PER_OUTPUT 8
static struct wl_display *display;
static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct zwlr_layer_shell_v1 *layers;
static struct ext_output_image_capture_source_manager_v1 *sources;
static struct ext_image_copy_capture_manager_v1 *captures;
static struct zwlr_screencopy_manager_v1 *legacy;

struct pixels {
    struct wl_buffer *buffer;
    unsigned char *map;
    size_t bytes;
    int width, height, stride, offset;
};
struct output {
    struct wl_output *output;
    struct wl_surface *surface;
    struct zwlr_layer_surface_v1 *layer;
    struct pixels background;
    int width, height, scale, configured;
    uint32_t color;
};
static struct output outputs[MAX_OUTPUTS];
static size_t output_count;

static void sync_display(void) { CHECK(wl_display_roundtrip(display) >= 0); }
static struct pixels make_pixels(int width, int height, int padded) {
    CHECK(width > 0 && height > 0 && width < 16384 && height < 16384);
    int stride = width * 4 + (padded ? 16 : 0), offset = padded ? 32 : 0;
    size_t bytes = (size_t)offset + (size_t)stride * height + 64;
    int fd = memfd_create("anvil-capture-batch", MFD_CLOEXEC);
    CHECK(fd >= 0 && ftruncate(fd, (off_t)bytes) == 0);
    unsigned char *map = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(map != MAP_FAILED);
    memset(map, 0xa5, bytes);
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int)bytes);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, offset, width, height, stride, WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool); close(fd);
    return (struct pixels){buffer, map, bytes, width, height, stride, offset};
}
static void free_pixels(struct pixels *p) { wl_buffer_destroy(p->buffer); munmap(p->map, p->bytes); }
static uint32_t *pixel_row(const struct pixels *p, int row) { return (uint32_t *)(p->map + p->offset + (size_t)row * p->stride); }
static void check_canaries(const struct pixels *p) {
    for (int i = 0; i < p->offset; i++) CHECK(p->map[i] == 0xa5);
    for (int y = 0; y < p->height; y++)
        for (int x = p->width * 4; x < p->stride; x++)
            CHECK(p->map[p->offset + (size_t)y * p->stride + x] == 0xa5);
    for (size_t i = (size_t)p->offset + (size_t)p->stride * p->height; i < p->bytes; i++) CHECK(p->map[i] == 0xa5);
}
static void output_geometry(void *d, struct wl_output *o, int32_t x, int32_t y, int32_t pw, int32_t ph, int32_t subpixel, const char *make, const char *model, int32_t transform) {}
static void output_mode(void *d, struct wl_output *o, uint32_t flags, int32_t w, int32_t h, int32_t refresh) {}
static void output_done(void *d, struct wl_output *o) {}
static void output_scale(void *d, struct wl_output *o, int32_t scale) { ((struct output *)d)->scale = scale; }
static const struct wl_output_listener output_listener = {.geometry=output_geometry, .mode=output_mode, .done=output_done, .scale=output_scale};
static void global(void *d, struct wl_registry *r, uint32_t name, const char *interface, uint32_t version) {
#define BIND(n, obj, max) if (!strcmp(interface, #n)) obj = wl_registry_bind(r, name, &n##_interface, version < max ? version : max)
    BIND(wl_compositor, compositor, 4);
    BIND(wl_shm, shm, 1);
    BIND(zwlr_layer_shell_v1, layers, 4);
    BIND(ext_output_image_capture_source_manager_v1, sources, 1);
    BIND(ext_image_copy_capture_manager_v1, captures, 1);
    BIND(zwlr_screencopy_manager_v1, legacy, 3);
#undef BIND
    if (!strcmp(interface, "wl_output")) {
        CHECK(output_count < MAX_OUTPUTS);
        struct output *o = &outputs[output_count++]; o->scale = 1;
        o->output = wl_registry_bind(r, name, &wl_output_interface, version < 2 ? version : 2);
        wl_output_add_listener(o->output, &output_listener, o);
    }
}
static void global_remove(void *d, struct wl_registry *r, uint32_t name) {}
static const struct wl_registry_listener registry_listener = {.global=global, .global_remove=global_remove};
static void paint_background(struct output *o) {
    for (int y = 0; y < o->height; y++)
        for (int x = 0; x < o->width; x++) pixel_row(&o->background, y)[x] = o->color;
    wl_surface_attach(o->surface, o->background.buffer, 0, 0);
    wl_surface_damage(o->surface, 0, 0, o->width, o->height); wl_surface_commit(o->surface);
}
static void layer_configure(void *d, struct zwlr_layer_surface_v1 *layer, uint32_t serial, uint32_t width, uint32_t height) {
    struct output *o = d;
    CHECK(!o->configured && width && height);
    zwlr_layer_surface_v1_ack_configure(layer, serial);
    o->width = (int)width; o->height = (int)height;
    o->background = make_pixels(o->width, o->height, 0);
    paint_background(o); o->configured = 1;
}
static void layer_closed(void *d, struct zwlr_layer_surface_v1 *layer) { CHECK(0 && "background output disappeared"); }
static const struct zwlr_layer_surface_v1_listener layer_listener = {.configure=layer_configure, .closed=layer_closed};

struct capture {
    struct output *output;
    struct ext_image_copy_capture_session_v1 *session;
    struct ext_image_copy_capture_frame_v1 *modern_frame;
    struct zwlr_screencopy_frame_v1 *legacy_frame;
    struct pixels pixels;
    int width, height, ready, failed, constraints, cropped, cursors;
    int crop_x, crop_y;
};
static void session_size(void *d, struct ext_image_copy_capture_session_v1 *s, uint32_t w, uint32_t h) { struct capture *c=d; c->width=(int)w; c->height=(int)h; }
static void session_format(void *d, struct ext_image_copy_capture_session_v1 *s, uint32_t format) { CHECK(format == WL_SHM_FORMAT_ARGB8888); }
static void session_device(void *d, struct ext_image_copy_capture_session_v1 *s, struct wl_array *device) { CHECK(0); }
static void session_dmabuf(void *d, struct ext_image_copy_capture_session_v1 *s, uint32_t format, struct wl_array *modifiers) { CHECK(0); }
static void session_done(void *d, struct ext_image_copy_capture_session_v1 *s) { ((struct capture *)d)->constraints=1; }
static void session_stopped(void *d, struct ext_image_copy_capture_session_v1 *s) { CHECK(0 && "capture session stopped"); }
static const struct ext_image_copy_capture_session_v1_listener session_listener = {.buffer_size=session_size, .shm_format=session_format, .dmabuf_device=session_device, .dmabuf_format=session_dmabuf, .done=session_done, .stopped=session_stopped};
static void modern_transform(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t transform) { CHECK(transform == WL_OUTPUT_TRANSFORM_NORMAL); }
static void modern_damage(void *d, struct ext_image_copy_capture_frame_v1 *f, int32_t x, int32_t y, int32_t w, int32_t h) { CHECK(w > 0 && h > 0); }
static void modern_time(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t hi, uint32_t lo, uint32_t nano) { CHECK(nano < 1000000000); }
static void modern_ready(void *d, struct ext_image_copy_capture_frame_v1 *f) { ((struct capture *)d)->ready=1; }
static void modern_failed(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t reason) { struct capture *c=d; c->failed=1; c->ready=1; }
static const struct ext_image_copy_capture_frame_v1_listener modern_listener = {.transform=modern_transform, .damage=modern_damage, .presentation_time=modern_time, .ready=modern_ready, .failed=modern_failed};
static void legacy_buffer(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t format, uint32_t width, uint32_t height, uint32_t stride) {
    struct capture *c=d; CHECK(format == WL_SHM_FORMAT_ARGB8888 && stride == width * 4);
    c->width=(int)width; c->height=(int)height; c->pixels=make_pixels(c->width,c->height,0);
}
static void legacy_flags(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t flags) { CHECK(flags == 0); }
static void legacy_ready(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t hi, uint32_t lo, uint32_t nano) { ((struct capture *)d)->ready=1; }
static void legacy_failed(void *d, struct zwlr_screencopy_frame_v1 *f) { struct capture *c=d; c->failed=1; c->ready=1; }
static void legacy_damage(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t x, uint32_t y, uint32_t width, uint32_t height) {}
static void legacy_dmabuf(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t format, uint32_t width, uint32_t height) { CHECK(0); }
static void legacy_done(void *d, struct zwlr_screencopy_frame_v1 *f) { ((struct capture *)d)->constraints=1; }
static const struct zwlr_screencopy_frame_v1_listener legacy_listener = {.buffer=legacy_buffer, .flags=legacy_flags, .ready=legacy_ready, .failed=legacy_failed, .damage=legacy_damage, .linux_dmabuf=legacy_dmabuf, .buffer_done=legacy_done};

static void create_capture(struct capture *c, struct output *o, int kind) {
    memset(c,0,sizeof(*c)); c->output=o; c->cursors=kind & 1;
    if (kind < 4) {
        struct ext_image_capture_source_v1 *source=ext_output_image_capture_source_manager_v1_create_source(sources,o->output);
        c->session=ext_image_copy_capture_manager_v1_create_session(captures,source,c->cursors ? EXT_IMAGE_COPY_CAPTURE_MANAGER_V1_OPTIONS_PAINT_CURSORS : 0);
        ext_image_copy_capture_session_v1_add_listener(c->session,&session_listener,c);
        ext_image_capture_source_v1_destroy(source);
    } else {
        c->cropped=kind >= 6;
        if (c->cropped) {
            c->crop_x=o->width/4; c->crop_y=o->height/4;
            c->legacy_frame=zwlr_screencopy_manager_v1_capture_output_region(legacy,c->cursors,o->output,c->crop_x,c->crop_y,o->width/2,o->height/2);
        } else c->legacy_frame=zwlr_screencopy_manager_v1_capture_output(legacy,c->cursors,o->output);
        zwlr_screencopy_frame_v1_add_listener(c->legacy_frame,&legacy_listener,c);
    }
}
static void submit_capture(struct capture *c) {
    CHECK(c->constraints && c->width > 0 && c->height > 0);
    if (c->session) {
        c->pixels=make_pixels(c->width,c->height,1);
        c->modern_frame=ext_image_copy_capture_session_v1_create_frame(c->session);
        ext_image_copy_capture_frame_v1_add_listener(c->modern_frame,&modern_listener,c);
        ext_image_copy_capture_frame_v1_attach_buffer(c->modern_frame,c->pixels.buffer);
        ext_image_copy_capture_frame_v1_damage_buffer(c->modern_frame,0,0,c->width,c->height);
        ext_image_copy_capture_frame_v1_capture(c->modern_frame);
    } else zwlr_screencopy_frame_v1_copy(c->legacy_frame,c->pixels.buffer);
}
static void destroy_capture(struct capture *c) {
    if (c->modern_frame) ext_image_copy_capture_frame_v1_destroy(c->modern_frame);
    if (c->session) ext_image_copy_capture_session_v1_destroy(c->session);
    if (c->legacy_frame) zwlr_screencopy_frame_v1_destroy(c->legacy_frame);
    free_pixels(&c->pixels);
}
static void check_capture(const struct capture *c) {
    CHECK(c->ready && !c->failed);
    size_t matched=0, total=(size_t)c->width*c->height;
    for (int y=0;y<c->height;y++)
        for (int x=0;x<c->width;x++)
            matched += pixel_row(&c->pixels,y)[x] == c->output->color;
    /* The compositor's bar and cursor may cover a small part of the background. */
    CHECK(matched > total*4/5);
    check_canaries(&c->pixels);
}
static void check_crop(const struct capture *crop, const struct capture *full) {
    int left=crop->crop_x*crop->output->scale, top=crop->crop_y*crop->output->scale;
    CHECK(left >= 0 && top >= 0 && left+crop->width <= full->width && top+crop->height <= full->height);
    for (int y=0;y<crop->height;y++)
        CHECK(!memcmp(pixel_row(&crop->pixels,y),pixel_row(&full->pixels,top+y)+left,(size_t)crop->width*4));
}
static void capture_batch(void) {
    struct capture batch[MAX_OUTPUTS][FRAMES_PER_OUTPUT], canceled;
    for (size_t i=0;i<output_count;i++)
        for (int k=0;k<FRAMES_PER_OUTPUT;k++) create_capture(&batch[i][k],&outputs[i],k);
    create_capture(&canceled,&outputs[0],0);
    sync_display();
    for (size_t i=0;i<output_count;i++)
        for (int k=0;k<FRAMES_PER_OUTPUT;k++) submit_capture(&batch[i][k]);
    submit_capture(&canceled);
    /* Destroy one submitted resource before flushing; every survivor must still complete. */
    ext_image_copy_capture_frame_v1_destroy(canceled.modern_frame); canceled.modern_frame=NULL;
    CHECK(wl_display_flush(display) >= 0);
    for (;;) {
        size_t done=0;
        for (size_t i=0;i<output_count;i++)
            for (int k=0;k<FRAMES_PER_OUTPUT;k++) done += batch[i][k].ready;
        if (done == output_count*FRAMES_PER_OUTPUT) break;
        CHECK(wl_display_dispatch(display) >= 0);
    }
    for (size_t i=0;i<output_count;i++) {
        for (int k=0;k<FRAMES_PER_OUTPUT;k++) check_capture(&batch[i][k]);
        check_crop(&batch[i][6],&batch[i][4]); check_crop(&batch[i][7],&batch[i][5]);
        /* Independent modern sessions must agree with the corresponding legacy full capture. */
        for (int k=0;k<4;k++) {
            const struct capture *a=&batch[i][k], *b=&batch[i][4+(k&1)];
            CHECK(a->width == b->width && a->height == b->height);
            for (int y=0;y<a->height;y++) CHECK(!memcmp(pixel_row(&a->pixels,y),pixel_row(&b->pixels,y),(size_t)a->width*4));
        }
        for (int k=0;k<FRAMES_PER_OUTPUT;k++) destroy_capture(&batch[i][k]);
    }
    destroy_capture(&canceled); sync_display();
}

int main(void) {
    display=wl_display_connect(NULL); CHECK(display);
    struct wl_registry *registry=wl_display_get_registry(display);
    wl_registry_add_listener(registry,&registry_listener,NULL); sync_display(); sync_display();
    CHECK(compositor && shm && layers && sources && captures && legacy && output_count);
    for (size_t i=0;i<output_count;i++) {
        struct output *o=&outputs[i];
        o->color=0xff000000u | (((uint32_t)i*71+173)&255)<<16 | (((uint32_t)i*97+31)&255)<<8 | (((uint32_t)i*43+67)&255);
        o->surface=wl_compositor_create_surface(compositor);
        o->layer=zwlr_layer_shell_v1_get_layer_surface(layers,o->surface,o->output,ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND,"anvil-capture-batch");
        zwlr_layer_surface_v1_add_listener(o->layer,&layer_listener,o);
        zwlr_layer_surface_v1_set_size(o->layer,0,0);
        zwlr_layer_surface_v1_set_anchor(o->layer,ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP|ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM|ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT|ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT);
        zwlr_layer_surface_v1_set_exclusive_zone(o->layer,-1); wl_surface_commit(o->surface);
    }
    sync_display(); sync_display();
    for (size_t i=0;i<output_count;i++) CHECK(outputs[i].configured);
    capture_batch();
    for (size_t i=0;i<output_count;i++) { outputs[i].color ^= 0x00ffffffu; paint_background(&outputs[i]); }
    sync_display(); capture_batch();
    printf("PASS: %zu outputs, parallel modern/legacy captures, cursor variants, exact crops, padded SHM, canceled frame and fresh pixels\n",output_count);
    for (size_t i=0;i<output_count;i++) {
        zwlr_layer_surface_v1_destroy(outputs[i].layer); wl_surface_destroy(outputs[i].surface); free_pixels(&outputs[i].background);
        wl_output_destroy(outputs[i].output);
    }
    wl_registry_destroy(registry); wl_display_disconnect(display); return 0;
}
