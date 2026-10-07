#define _GNU_SOURCE
#include <wayland-client.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include "xdg-shell-client.h"
#include "layer-shell-client.h"
#include "image-source-client.h"
#include "image-copy-client.h"
#include "session-lock-client.h"

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #c); exit(1); } } while (0)
static struct wl_display *display;
static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct wl_output *output;
static struct xdg_wm_base *wm;
static struct zwlr_layer_shell_v1 *layers;
static struct ext_output_image_capture_source_manager_v1 *sources;
static struct ext_image_copy_capture_manager_v1 *captures;
static struct ext_session_lock_manager_v1 *locks;
static int layer_expected;
static struct wl_seat *seat;
static struct wl_keyboard *keyboard;
static struct wl_surface *keyboard_focus;
static void keymap(void *d, struct wl_keyboard *k, uint32_t format, int32_t fd, uint32_t size) { close(fd); }
static void key_enter(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *surface, struct wl_array *keys) { keyboard_focus=surface; }
static void key_leave(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *surface) { keyboard_focus=NULL; }
static void key(void *d, struct wl_keyboard *k, uint32_t serial, uint32_t time, uint32_t code, uint32_t state) {}
static void modifiers(void *d, struct wl_keyboard *k, uint32_t serial, uint32_t depressed, uint32_t latched, uint32_t locked_mods, uint32_t group) {}
static const struct wl_keyboard_listener keyboard_listener = {.keymap=keymap, .enter=key_enter, .leave=key_leave, .key=key, .modifiers=modifiers};
static uint32_t source_width, source_height;
static int constraints_done, stopped, frame_done, frame_failed, locked;
static uint32_t failure_reason;

struct pixels { struct wl_buffer *buffer; uint32_t *map; size_t bytes; int width, height; };
static struct pixels make_pixels(int width, int height, uint32_t color) {
    CHECK(width > 0 && height > 0 && width < 16384 && height < 16384);
    size_t bytes = (size_t)width * (size_t)height * 4;
    int fd = memfd_create("anvil-protocol-test", MFD_CLOEXEC);
    CHECK(fd >= 0 && ftruncate(fd, (off_t)bytes) == 0);
    uint32_t *map = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(map != MAP_FAILED);
    for (size_t i = 0; i < bytes / 4; ++i) map[i] = color;
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int)bytes);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, width, height, width * 4, WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool); close(fd);
    return (struct pixels){buffer, map, bytes, width, height};
}
static void free_pixels(struct pixels *pixels) { wl_buffer_destroy(pixels->buffer); munmap(pixels->map, pixels->bytes); }
static void sync_display(void) { CHECK(wl_display_roundtrip(display) >= 0); }
static void wait_flag(int *flag) { while (!*flag) CHECK(wl_display_dispatch(display) >= 0); }
static void wm_ping(void *d, struct xdg_wm_base *base, uint32_t serial) { xdg_wm_base_pong(base, serial); }
static const struct xdg_wm_base_listener wm_listener = {.ping=wm_ping};
static void global(void *d, struct wl_registry *r, uint32_t name, const char *interface, uint32_t version) {
#define BIND(n, obj, max) if (!strcmp(interface, #n)) obj = wl_registry_bind(r, name, &n##_interface, version < max ? version : max)
    BIND(wl_compositor, compositor, 4);
    BIND(wl_shm, shm, 1);
    BIND(wl_seat, seat, 1);
    BIND(wl_output, output, 1);
    BIND(xdg_wm_base, wm, 1);
    BIND(zwlr_layer_shell_v1, layers, 4);
    BIND(ext_output_image_capture_source_manager_v1, sources, 1);
    BIND(ext_image_copy_capture_manager_v1, captures, 1);
    BIND(ext_session_lock_manager_v1, locks, 1);
#undef BIND
}
static void global_remove(void *d, struct wl_registry *r, uint32_t name) {}
static const struct wl_registry_listener registry_listener = {.global=global, .global_remove=global_remove};
struct window { struct wl_surface *surface; struct xdg_surface *xdg; struct xdg_toplevel *top; int width, height, configured; struct pixels pixels; int has_pixels; };
static void top_configure(void *d, struct xdg_toplevel *top, int32_t width, int32_t height, struct wl_array *states) {
    struct window *w = d; if (width > 0) w->width=width; if (height > 0) w->height=height;
}
static void top_close(void *d, struct xdg_toplevel *top) { CHECK(0 && "unexpected window close"); }
static const struct xdg_toplevel_listener top_listener = {.configure=top_configure, .close=top_close};
static void window_configure(void *d, struct xdg_surface *surface, uint32_t serial) {
    struct window *w = d; xdg_surface_ack_configure(surface, serial);
    // Keep the few previous buffers alive until disconnect: tests are short and resize must not
    // race renderer use of the previous attachment.
    w->pixels=make_pixels(w->width, w->height, 0xffff0000); w->has_pixels=1;
    wl_surface_attach(w->surface, w->pixels.buffer, 0, 0);
    wl_surface_damage(w->surface, 0, 0, w->width, w->height);
    wl_surface_commit(w->surface); w->configured++;
}
static const struct xdg_surface_listener window_listener = {.configure=window_configure};
static void create_window(struct window *w) {
    memset(w, 0, sizeof(*w)); w->width=320; w->height=240;
    w->surface=wl_compositor_create_surface(compositor);
    w->xdg=xdg_wm_base_get_xdg_surface(wm, w->surface);
    xdg_surface_add_listener(w->xdg, &window_listener, w);
    w->top=xdg_surface_get_toplevel(w->xdg);
    xdg_toplevel_add_listener(w->top, &top_listener, w);
    xdg_toplevel_set_title(w->top, "Anvil protocol test");
    xdg_toplevel_set_app_id(w->top, "anvil-test");
    wl_surface_commit(w->surface);
    wait_flag(&w->configured); sync_display();
}
struct panel { struct wl_surface *surface; struct zwlr_layer_surface_v1 *role; struct pixels pixels; int configured; };
static void panel_configure(void *d, struct zwlr_layer_surface_v1 *role, uint32_t serial, uint32_t width, uint32_t height) {
    struct panel *p=d; zwlr_layer_surface_v1_ack_configure(role, serial);
    CHECK(width > 0 && height == 64);
    p->pixels=make_pixels((int)width, (int)height, 0xff0000ff);
    wl_surface_attach(p->surface, p->pixels.buffer, 0, 0);
    wl_surface_damage(p->surface, 0, 0, (int)width, (int)height);
    wl_surface_commit(p->surface); p->configured++;
}
static void panel_closed(void *d, struct zwlr_layer_surface_v1 *role) { CHECK(0 && "unexpected panel close"); }
static const struct zwlr_layer_surface_v1_listener panel_listener = {.configure=panel_configure, .closed=panel_closed};
static void create_panel(struct panel *p) {
    memset(p, 0, sizeof(*p)); p->surface=wl_compositor_create_surface(compositor);
    p->role=zwlr_layer_shell_v1_get_layer_surface(layers, p->surface, output, ZWLR_LAYER_SHELL_V1_LAYER_TOP, "anvil-test-panel");
    zwlr_layer_surface_v1_add_listener(p->role, &panel_listener, p);
    zwlr_layer_surface_v1_set_size(p->role, 0, 64);
    zwlr_layer_surface_v1_set_anchor(p->role, ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP | ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT | ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT);
    zwlr_layer_surface_v1_set_exclusive_zone(p->role, 64);
    wl_surface_commit(p->surface); wait_flag(&p->configured); sync_display();
}
static void session_size(void *d, struct ext_image_copy_capture_session_v1 *s, uint32_t width, uint32_t height) { source_width=width; source_height=height; }
static void session_format(void *d, struct ext_image_copy_capture_session_v1 *s, uint32_t format) { CHECK(format == WL_SHM_FORMAT_ARGB8888); }
static void session_device(void *d, struct ext_image_copy_capture_session_v1 *s, struct wl_array *device) { CHECK(0); }
static void session_dmabuf(void *d, struct ext_image_copy_capture_session_v1 *s, uint32_t format, struct wl_array *modifiers) { CHECK(0); }
static void session_done(void *d, struct ext_image_copy_capture_session_v1 *s) { constraints_done=1; }
static void session_stopped(void *d, struct ext_image_copy_capture_session_v1 *s) { stopped=1; }
static const struct ext_image_copy_capture_session_v1_listener session_listener = {.buffer_size=session_size, .shm_format=session_format, .dmabuf_device=session_device, .dmabuf_format=session_dmabuf, .done=session_done, .stopped=session_stopped};
static void frame_transform(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t transform) { CHECK(transform == WL_OUTPUT_TRANSFORM_NORMAL); }
static void frame_damage(void *d, struct ext_image_copy_capture_frame_v1 *f, int32_t x, int32_t y, int32_t width, int32_t height) { CHECK(width > 0 && height > 0); }
static void frame_time(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t hi, uint32_t lo, uint32_t nano) { CHECK(nano < 1000000000); }
static void frame_ready(void *d, struct ext_image_copy_capture_frame_v1 *f) { frame_done=1; }
static void frame_failure(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t reason) { frame_failed=1; failure_reason=reason; frame_done=1; }
static const struct ext_image_copy_capture_frame_v1_listener frame_listener = {.transform=frame_transform, .damage=frame_damage, .presentation_time=frame_time, .ready=frame_ready, .failed=frame_failure};
static struct ext_image_copy_capture_session_v1 *new_session(void) {
    struct ext_image_capture_source_v1 *source=ext_output_image_capture_source_manager_v1_create_source(sources, output);
    struct ext_image_copy_capture_session_v1 *session=ext_image_copy_capture_manager_v1_create_session(captures, source, 0);
    ext_image_copy_capture_session_v1_add_listener(session, &session_listener, NULL);
    ext_image_capture_source_v1_destroy(source);
    sync_display(); return session;
}
static struct pixels capture(struct ext_image_copy_capture_session_v1 *session, int wrong_size, int expected_failure) {
    CHECK(source_width && source_height);
    struct pixels pixels=make_pixels((int)source_width - wrong_size, (int)source_height, 0xff123456);
    struct ext_image_copy_capture_frame_v1 *frame=ext_image_copy_capture_session_v1_create_frame(session);
    ext_image_copy_capture_frame_v1_add_listener(frame, &frame_listener, NULL);
    frame_done=frame_failed=0;
    ext_image_copy_capture_frame_v1_attach_buffer(frame, pixels.buffer);
    ext_image_copy_capture_frame_v1_damage_buffer(frame, 0, 0, pixels.width, pixels.height);
    ext_image_copy_capture_frame_v1_capture(frame);
    wait_flag(&frame_done); CHECK(frame_failed == expected_failure);
    if (wrong_size) CHECK(failure_reason == EXT_IMAGE_COPY_CAPTURE_FRAME_V1_FAILURE_REASON_BUFFER_CONSTRAINTS);
    ext_image_copy_capture_frame_v1_destroy(frame); sync_display(); return pixels;
}
static size_t color_count(const struct pixels *pixels, uint32_t color) { size_t count=0; for (size_t i=0; i<pixels->bytes/4; i++) if (pixels->map[i] == color) count++; return count; }
static void lock_locked(void *d, struct ext_session_lock_v1 *lock) { locked=1; }
static void lock_finished(void *d, struct ext_session_lock_v1 *lock) { CHECK(0 && "lock rejected"); }
static const struct ext_session_lock_v1_listener lock_listener = {.locked=lock_locked, .finished=lock_finished};
int main(int argc, char **argv) {
    layer_expected=argc > 1 && !strcmp(argv[1], "layer-shell");
    display=wl_display_connect(NULL); CHECK(display);
    struct wl_registry *registry=wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL); sync_display();
    CHECK(compositor && shm && output && wm && sources && captures && locks);
    CHECK((layers != NULL) == layer_expected);
    xdg_wm_base_add_listener(wm, &wm_listener, NULL);
    CHECK(seat); keyboard=wl_seat_get_keyboard(seat); wl_keyboard_add_listener(keyboard, &keyboard_listener, NULL);
    struct window window; create_window(&window);
    sync_display(); CHECK(keyboard_focus == window.surface);
    if (argc > 2 && !strcmp(argv[2], "hold")) {
        puts("READY"); fflush(stdout);
        while (wl_display_dispatch(display) >= 0) {}
        CHECK(0 && "unexpected compositor disconnect");
    }
    // A second real toplevel must retile the first and take keyboard focus. Destroying it
    // restores the first window's size and focus without reconnecting the surviving client.
    int original_width=window.width;
    struct window second; create_window(&second); sync_display();
    CHECK(window.width < original_width && keyboard_focus == second.surface);
    xdg_toplevel_destroy(second.top); xdg_surface_destroy(second.xdg); wl_surface_destroy(second.surface);
    sync_display(); sync_display(); CHECK(window.width == original_width && keyboard_focus == window.surface);
    if (second.has_pixels) free_pixels(&second.pixels);
    int original_height=window.height;
    struct ext_image_copy_capture_session_v1 *session=new_session(); CHECK(constraints_done && !stopped);
    struct pixels pixels=capture(session, 0, 0); CHECK(color_count(&pixels, 0xffff0000) > 1000); free_pixels(&pixels);
    pixels=capture(session, 1, 1); free_pixels(&pixels);
    // Reuse a session after destroying a failed frame.
    pixels=capture(session, 0, 0); CHECK(color_count(&pixels, 0xffff0000) > 1000); free_pixels(&pixels);
    if (layer_expected) {
        struct panel panel; create_panel(&panel);
        sync_display(); CHECK(window.height < original_height);
        zwlr_layer_surface_v1_set_keyboard_interactivity(panel.role, ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_EXCLUSIVE); wl_surface_commit(panel.surface); sync_display(); sync_display(); CHECK(keyboard_focus == panel.surface);
        zwlr_layer_surface_v1_set_keyboard_interactivity(panel.role, ZWLR_LAYER_SURFACE_V1_KEYBOARD_INTERACTIVITY_NONE); wl_surface_commit(panel.surface); sync_display(); sync_display(); CHECK(keyboard_focus == window.surface);
        pixels=capture(session, 0, 0); CHECK(color_count(&pixels, 0xff0000ff) >= source_width * 32); CHECK(color_count(&pixels, 0xffff0000) > 1000); free_pixels(&pixels);
        zwlr_layer_surface_v1_set_exclusive_zone(panel.role, 0); wl_surface_commit(panel.surface); sync_display(); sync_display(); CHECK(window.height == original_height);
        zwlr_layer_surface_v1_set_exclusive_zone(panel.role, 64); wl_surface_commit(panel.surface); sync_display(); sync_display(); CHECK(window.height < original_height);
        zwlr_layer_surface_v1_destroy(panel.role); wl_surface_destroy(panel.surface); sync_display(); sync_display(); CHECK(window.height == original_height);
        free_pixels(&panel.pixels);
        pixels=capture(session, 0, 0); CHECK(color_count(&pixels, 0xff0000ff) == 0); free_pixels(&pixels);
    }
    struct ext_session_lock_v1 *lock=ext_session_lock_manager_v1_lock(locks);
    ext_session_lock_v1_add_listener(lock, &lock_listener, NULL); wait_flag(&locked); sync_display(); CHECK(stopped);
    pixels=capture(session, 0, 1); CHECK(failure_reason == EXT_IMAGE_COPY_CAPTURE_FRAME_V1_FAILURE_REASON_STOPPED); free_pixels(&pixels);
    ext_session_lock_v1_unlock_and_destroy(lock); sync_display();
    ext_image_copy_capture_session_v1_destroy(session);
    stopped=constraints_done=0; session=new_session(); CHECK(constraints_done && !stopped);
    pixels=capture(session, 0, 0); CHECK(color_count(&pixels, 0xffff0000) > 1000); free_pixels(&pixels);
    ext_image_copy_capture_session_v1_destroy(session);
    xdg_toplevel_destroy(window.top); xdg_surface_destroy(window.xdg); wl_surface_destroy(window.surface); sync_display();
    if (window.has_pixels) free_pixels(&window.pixels);
    wl_display_disconnect(display);
    puts("PASS: window lifecycle, actual capture pixels, buffer validation, session reuse, lock isolation, layer feature/exclusive zones");
    return 0;
}
