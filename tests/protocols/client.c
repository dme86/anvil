#define _GNU_SOURCE
#include <wayland-client.h>
#include <stdint.h>
#include <poll.h>
#include <time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#include "xdg-shell-client.h"
#include "layer-shell-client.h"
#include "image-source-client.h"
#include "image-copy-client.h"
#include "session-lock-client.h"
#include "primary-selection-client.h"
#include "pointer-constraints-client.h"
#include "idle-notify-client.h"
#include "idle-inhibit-client.h"

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
static struct wl_pointer *pointer;
static struct wl_data_device_manager *data_manager;
static struct zwp_primary_selection_device_manager_v1 *primary_manager;
static struct zwp_pointer_constraints_v1 *pointer_constraints;
static struct ext_idle_notifier_v1 *idle_notifier;
static struct zwp_idle_inhibit_manager_v1 *idle_inhibit_manager;
static uint32_t focus_serial;
static int watch_motion;
static int watch_layout;
static int layout_configures, layout_enters, layout_leaves;
static struct wl_surface *keyboard_focus;
static void keymap(void *d, struct wl_keyboard *k, uint32_t format, int32_t fd, uint32_t size) { close(fd); }
static void key_enter(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *surface, struct wl_array *keys) { keyboard_focus=surface; focus_serial=serial; }
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
    BIND(wl_data_device_manager, data_manager, 3);
    BIND(zwp_primary_selection_device_manager_v1, primary_manager, 1);
    BIND(zwp_pointer_constraints_v1, pointer_constraints, 1);
    BIND(ext_idle_notifier_v1, idle_notifier, 2);
    BIND(zwp_idle_inhibit_manager_v1, idle_inhibit_manager, 1);
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
struct window { struct wl_surface *surface; struct xdg_surface *xdg; struct xdg_toplevel *top; int width, height, configured; struct pixels pixels; int has_pixels; int defer_buffer; };
static void top_configure(void *d, struct xdg_toplevel *top, int32_t width, int32_t height, struct wl_array *states) {
    layout_configures++;
    if (watch_layout) { printf("CONFIG %d %d\n",width,height); fflush(stdout); }
    struct window *w = d; if (width > 0) w->width=width; if (height > 0) w->height=height;
}
static void top_close(void *d, struct xdg_toplevel *top) { CHECK(0 && "unexpected window close"); }
static const struct xdg_toplevel_listener top_listener = {.configure=top_configure, .close=top_close};
static void paint_window(struct window *w) {
    struct pixels old=w->pixels; int had_pixels=w->has_pixels;
    w->pixels=make_pixels(w->width, w->height, 0xffff0000); w->has_pixels=1;
    wl_surface_attach(w->surface, w->pixels.buffer, 0, 0);
    wl_surface_damage(w->surface, 0, 0, w->width, w->height);
    wl_surface_commit(w->surface);
    if (had_pixels) free_pixels(&old);
}
static void window_configure(void *d, struct xdg_surface *surface, uint32_t serial) {
    struct window *w = d; xdg_surface_ack_configure(surface, serial);
    w->configured++;
    if (!w->defer_buffer) paint_window(w);
}
static const struct xdg_surface_listener window_listener = {.configure=window_configure};
static void surface_enter(void *d, struct wl_surface *surface, struct wl_output *entered) {
    layout_enters++;
    if (watch_layout) { puts("ENTER"); fflush(stdout); }
}
static void surface_leave(void *d, struct wl_surface *surface, struct wl_output *left) {
    layout_leaves++;
    if (watch_layout) { puts("LEAVE"); fflush(stdout); }
}
static const struct wl_surface_listener surface_listener = {.enter=surface_enter, .leave=surface_leave};
static void create_window_mode(struct window *w, int defer_buffer) {
    memset(w, 0, sizeof(*w)); w->width=320; w->height=240; w->defer_buffer=defer_buffer;
    w->surface=wl_compositor_create_surface(compositor);
    wl_surface_add_listener(w->surface,&surface_listener,w);
    w->xdg=xdg_wm_base_get_xdg_surface(wm, w->surface);
    xdg_surface_add_listener(w->xdg, &window_listener, w);
    w->top=xdg_surface_get_toplevel(w->xdg);
    xdg_toplevel_add_listener(w->top, &top_listener, w);
    xdg_toplevel_set_title(w->top, "Anvil protocol test");
    xdg_toplevel_set_app_id(w->top, "anvil-test");
    wl_surface_commit(w->surface);
    wait_flag(&w->configured); sync_display();
}
static void create_window(struct window *w) { create_window_mode(w, 0); }
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

static void destroy_window(struct window *w) {
    xdg_toplevel_destroy(w->top); xdg_surface_destroy(w->xdg); wl_surface_destroy(w->surface);
    sync_display(); if (w->has_pixels) free_pixels(&w->pixels);
}
struct popup { struct wl_surface *surface; struct xdg_surface *xdg; struct xdg_popup *role; struct pixels pixels; int configured; };
static void popup_configure(void *d, struct xdg_popup *role, int32_t x, int32_t y, int32_t width, int32_t height) { CHECK(width > 0 && height > 0); }
static void popup_done(void *d, struct xdg_popup *role) {}
static const struct xdg_popup_listener popup_listener={.configure=popup_configure,.popup_done=popup_done};
static void popup_surface_configure(void *d, struct xdg_surface *surface, uint32_t serial) {
    struct popup *p=d; xdg_surface_ack_configure(surface,serial);
    if (!p->configured) p->pixels=make_pixels(32,32,0xff00ff00);
    wl_surface_attach(p->surface,p->pixels.buffer,0,0); wl_surface_damage(p->surface,0,0,32,32); wl_surface_commit(p->surface); p->configured++;
}
static const struct xdg_surface_listener popup_surface_listener={.configure=popup_surface_configure};
static void create_popup(struct popup *p, struct window *parent) {
    memset(p,0,sizeof(*p)); p->surface=wl_compositor_create_surface(compositor);
    p->xdg=xdg_wm_base_get_xdg_surface(wm,p->surface);
    xdg_surface_add_listener(p->xdg,&popup_surface_listener,p);
    struct xdg_positioner *positioner=xdg_wm_base_create_positioner(wm);
    xdg_positioner_set_size(positioner,32,32); xdg_positioner_set_anchor_rect(positioner,10,10,1,1);
    p->role=xdg_surface_get_popup(p->xdg,parent->xdg,positioner); xdg_positioner_destroy(positioner);
    xdg_popup_add_listener(p->role,&popup_listener,p); wl_surface_commit(p->surface); wait_flag(&p->configured); sync_display();
}
static void destroy_popup(struct popup *p) {
    xdg_popup_destroy(p->role); xdg_surface_destroy(p->xdg); wl_surface_destroy(p->surface);
    sync_display(); free_pixels(&p->pixels);
}
static void pointer_enter(void *d, struct wl_pointer *p, uint32_t serial, struct wl_surface *surface, wl_fixed_t x, wl_fixed_t y) {
    if (watch_motion) { printf("MOTION %d %d\n",wl_fixed_to_int(x),wl_fixed_to_int(y)); fflush(stdout); }
}
static void pointer_leave(void *d, struct wl_pointer *p, uint32_t serial, struct wl_surface *surface) {}
static void pointer_motion(void *d, struct wl_pointer *p, uint32_t time, wl_fixed_t x, wl_fixed_t y) {
    if (watch_motion) { printf("MOTION %d %d\n",wl_fixed_to_int(x),wl_fixed_to_int(y)); fflush(stdout); }
}
static void pointer_button(void *d, struct wl_pointer *p, uint32_t serial, uint32_t time, uint32_t button, uint32_t state) {}
static void pointer_axis(void *d, struct wl_pointer *p, uint32_t time, uint32_t axis, wl_fixed_t value) {}
static const struct wl_pointer_listener pointer_listener={.enter=pointer_enter,.leave=pointer_leave,.motion=pointer_motion,.button=pointer_button,.axis=pointer_axis};
static void source_target(void *d, struct wl_data_source *s, const char *mime) {}
static void source_send(void *d, struct wl_data_source *s, const char *mime, int32_t fd) { CHECK(write(fd,"anvil-test",10)==10); close(fd); }
static void source_cancelled(void *d, struct wl_data_source *s) {}
static const struct wl_data_source_listener source_listener={.target=source_target,.send=source_send,.cancelled=source_cancelled};
static void offer_mime(void *d, struct wl_data_offer *offer, const char *mime) {}
static const struct wl_data_offer_listener offer_listener={.offer=offer_mime};
static void offer(void *d, struct wl_data_device *device, struct wl_data_offer *offer) { wl_data_offer_add_listener(offer,&offer_listener,NULL); }
static void selection(void *d, struct wl_data_device *device, struct wl_data_offer *offer) { if (offer) wl_data_offer_destroy(offer); }
static const struct wl_data_device_listener device_listener={.data_offer=offer,.selection=selection};
static void primary_send(void *d, struct zwp_primary_selection_source_v1 *s, const char *mime, int32_t fd) { CHECK(write(fd,"anvil-test",10)==10); close(fd); }
static void primary_cancelled(void *d, struct zwp_primary_selection_source_v1 *s) {}
static const struct zwp_primary_selection_source_v1_listener primary_source_listener={.send=primary_send,.cancelled=primary_cancelled};
static void primary_mime(void *d, struct zwp_primary_selection_offer_v1 *offer, const char *mime) {}
static const struct zwp_primary_selection_offer_v1_listener primary_offer_listener={.offer=primary_mime};
static void primary_offer(void *d, struct zwp_primary_selection_device_v1 *device, struct zwp_primary_selection_offer_v1 *offer) { zwp_primary_selection_offer_v1_add_listener(offer,&primary_offer_listener,NULL); }
static void primary_selection(void *d, struct zwp_primary_selection_device_v1 *device, struct zwp_primary_selection_offer_v1 *offer) { if (offer) zwp_primary_selection_offer_v1_destroy(offer); }
static const struct zwp_primary_selection_device_v1_listener primary_device_listener={.data_offer=primary_offer,.selection=primary_selection};
static void pointer_locked(void *d, struct zwp_locked_pointer_v1 *p) { puts("LOCKED"); fflush(stdout); }
static void pointer_unlocked(void *d, struct zwp_locked_pointer_v1 *p) {}
static const struct zwp_locked_pointer_v1_listener locked_pointer_listener={.locked=pointer_locked,.unlocked=pointer_unlocked};
static void test_locked(void *d, struct zwp_locked_pointer_v1 *p) {}
static void test_unlocked(void *d, struct zwp_locked_pointer_v1 *p) {}
static const struct zwp_locked_pointer_v1_listener test_lock_listener={.locked=test_locked,.unlocked=test_unlocked};
static void test_confined(void *d, struct zwp_confined_pointer_v1 *p) {}
static void test_unconfined(void *d, struct zwp_confined_pointer_v1 *p) {}
static const struct zwp_confined_pointer_v1_listener test_confine_listener={.confined=test_confined,.unconfined=test_unconfined};
static void own_protocol_state(struct window *window) {
    CHECK(data_manager && primary_manager && pointer_constraints && focus_serial);
    struct wl_data_device *device=wl_data_device_manager_get_data_device(data_manager,seat);
    wl_data_device_add_listener(device,&device_listener,NULL);
    struct wl_data_source *source=wl_data_device_manager_create_data_source(data_manager);
    wl_data_source_add_listener(source,&source_listener,NULL); wl_data_source_offer(source,"text/plain;charset=utf-8");
    wl_data_device_set_selection(device,source,focus_serial);
    struct zwp_primary_selection_device_v1 *primary=zwp_primary_selection_device_manager_v1_get_device(primary_manager,seat);
    zwp_primary_selection_device_v1_add_listener(primary,&primary_device_listener,NULL);
    struct zwp_primary_selection_source_v1 *primary_source=zwp_primary_selection_device_manager_v1_create_source(primary_manager);
    zwp_primary_selection_source_v1_add_listener(primary_source,&primary_source_listener,NULL);
    zwp_primary_selection_source_v1_offer(primary_source,"text/plain;charset=utf-8");
    zwp_primary_selection_device_v1_set_selection(primary,primary_source,focus_serial);
    struct zwp_locked_pointer_v1 *lock=zwp_pointer_constraints_v1_lock_pointer(pointer_constraints,window->surface,pointer,NULL,ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_PERSISTENT);
    zwp_locked_pointer_v1_add_listener(lock,&locked_pointer_listener,NULL); sync_display();
}
/* Interactive fixture for timing and inhibition checks from an external input driver. */
struct idle_watch { struct ext_idle_notification_v1 *notification; unsigned id; int idle; };
static uint64_t monotonic_ms(void) {
    struct timespec now; CHECK(clock_gettime(CLOCK_MONOTONIC,&now)==0);
    return (uint64_t)now.tv_sec*1000+(uint64_t)now.tv_nsec/1000000;
}
static void idle_idled(void *d, struct ext_idle_notification_v1 *notification) {
    struct idle_watch *watch=d; CHECK(!watch->idle); watch->idle=1;
    printf("IDLED %u %llu\n",watch->id,(unsigned long long)monotonic_ms()); fflush(stdout);
}
static void idle_resumed(void *d, struct ext_idle_notification_v1 *notification) {
    struct idle_watch *watch=d; CHECK(watch->idle); watch->idle=0;
    printf("RESUMED %u %llu\n",watch->id,(unsigned long long)monotonic_ms()); fflush(stdout);
}
static const struct ext_idle_notification_v1_listener idle_listener={.idled=idle_idled,.resumed=idle_resumed};
static void idle_interactive(void) {
    CHECK(idle_notifier && idle_inhibit_manager);
    CHECK(setvbuf(stdin,NULL,_IONBF,0)==0);
    CHECK(wl_proxy_get_version((struct wl_proxy *)idle_notifier)>=2);
    struct idle_watch watches[16]={0};
    struct window window; create_window(&window);
    struct zwp_idle_inhibitor_v1 *inhibitor=NULL;
    sync_display(); puts("READY"); fflush(stdout);
    for (;;) {
        CHECK(wl_display_dispatch_pending(display)>=0); CHECK(wl_display_flush(display)>=0);
        struct pollfd fds[2]={{wl_display_get_fd(display),POLLIN,0},{STDIN_FILENO,POLLIN,0}};
        CHECK(poll(fds,2,-1)>=0);
        if (fds[0].revents&POLLIN) CHECK(wl_display_dispatch(display)>=0);
        if (fds[1].revents&(POLLIN|POLLHUP)) {
            char command[128],kind[16]; unsigned id,timeout;
            if (!fgets(command,sizeof(command),stdin) || !strncmp(command,"quit",4)) break;
            if (sscanf(command,"add %u %15s %u",&id,kind,&timeout)==3) {
                CHECK(id<16 && !watches[id].notification);
                struct idle_watch *watch=&watches[id]; watch->id=id; watch->idle=0;
                if (!strcmp(kind,"normal")) watch->notification=ext_idle_notifier_v1_get_idle_notification(idle_notifier,timeout,seat);
                else { CHECK(!strcmp(kind,"input")); watch->notification=ext_idle_notifier_v1_get_input_idle_notification(idle_notifier,timeout,seat); }
                ext_idle_notification_v1_add_listener(watch->notification,&idle_listener,watch);
            } else if (sscanf(command,"del %u",&id)==1) {
                CHECK(id<16 && watches[id].notification);
                ext_idle_notification_v1_destroy(watches[id].notification); watches[id].notification=NULL;
            } else if (!strncmp(command,"inhibit",7)) {
                CHECK(window.surface && !inhibitor);
                inhibitor=zwp_idle_inhibit_manager_v1_create_inhibitor(idle_inhibit_manager,window.surface);
            } else if (!strncmp(command,"uninhibit",9)) {
                CHECK(inhibitor); zwp_idle_inhibitor_v1_destroy(inhibitor); inhibitor=NULL;
            } else if (!strncmp(command,"destroy-window",14)) {
                CHECK(window.surface); destroy_window(&window); memset(&window,0,sizeof(window));
            } else if (!strncmp(command,"new-window",10)) {
                CHECK(!window.surface); create_window(&window);
            } else { CHECK(!strncmp(command,"sync",4)); }
            sync_display();
            printf("ACK %llu\n",(unsigned long long)monotonic_ms()); fflush(stdout);
        }
    }
    for (unsigned i=0;i<16;i++) if (watches[i].notification) ext_idle_notification_v1_destroy(watches[i].notification);
    if (inhibitor) zwp_idle_inhibitor_v1_destroy(inhibitor);
    if (window.surface) destroy_window(&window);
}

static void stress_windows(int iterations) {
    for (int i=0;i<iterations;i++) {
        struct window first, second; create_window(&first); create_window(&second); sync_display();
        CHECK(keyboard_focus==second.surface);
        xdg_toplevel_set_fullscreen(second.top,output); sync_display();
        struct popup popup; create_popup(&popup,&second); destroy_popup(&popup);
        xdg_toplevel_unset_fullscreen(second.top); sync_display();
        destroy_window(&second); sync_display(); CHECK(keyboard_focus==first.surface);
        destroy_window(&first); sync_display(); CHECK(keyboard_focus==NULL);
    }
    printf("PASS: %d window pairs, configure/resize, focus, fullscreen requests and popups\n",iterations);
}

int main(int argc, char **argv) {
    layer_expected=argc > 1 && !strcmp(argv[1], "layer-shell");
    display=wl_display_connect(NULL); CHECK(display);
    struct wl_registry *registry=wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL); sync_display();
    CHECK(compositor && shm && output && wm && sources && captures && locks);
    CHECK((layers != NULL) == layer_expected);
    xdg_wm_base_add_listener(wm, &wm_listener, NULL);
    CHECK(seat); keyboard=wl_seat_get_keyboard(seat); wl_keyboard_add_listener(keyboard, &keyboard_listener, NULL);
    pointer=wl_seat_get_pointer(seat); wl_pointer_add_listener(pointer,&pointer_listener,NULL);
    if (argc > 2 && !strcmp(argv[2], "idle-watch")) {
        idle_interactive(); wl_display_disconnect(display); return 0;
    }
    if (argc > 2 && !strcmp(argv[2], "lock-owner")) {
        struct ext_session_lock_v1 *lock=ext_session_lock_manager_v1_lock(locks);
        ext_session_lock_v1_add_listener(lock,&lock_listener,NULL); wait_flag(&locked);
        puts("READY"); fflush(stdout); while (wl_display_dispatch(display)>=0) {}
        CHECK(0 && "unexpected disconnect");
    }
    if (argc > 2 && !strcmp(argv[2], "stress")) {
        CHECK(argc==4); char *end; long count=strtol(argv[3],&end,10); CHECK(*end==0 && count>0 && count<=1000000);
        stress_windows((int)count); wl_display_disconnect(display); return 0;
    }
    if (argc > 2 && !strcmp(argv[2], "layout-noop")) {
        struct window windows[12];
        for (int i=0;i<12;i++) create_window(&windows[i]);
        sync_display(); sync_display();
        int configured=layout_configures, entered=layout_enters, left=layout_leaves;
        const char *ctl=getenv("ANVILCTL"); if (!ctl) ctl="anvilctl";
        for (int i=0;i<20;i++) {
            pid_t child=fork(); CHECK(child>=0);
            if (!child) { CHECK(freopen("/dev/null","w",stdout)); execlp(ctl,ctl,"reload",(char *)NULL); _exit(127); }
            int status; CHECK(waitpid(child,&status,0)==child && WIFEXITED(status) && WEXITSTATUS(status)==0);
            sync_display(); sync_display();
            CHECK(layout_configures==configured && layout_enters==entered && layout_leaves==left);
        }
        puts("PASS: 20 unchanged layouts with 12 windows, no configure or output leave/enter churn");
        wl_display_disconnect(display); return 0;
    }
    if (argc > 2 && !strcmp(argv[2], "layout-watch")) {
        watch_layout=1;
        struct window windows[12];
        for (int i=0;i<12;i++) create_window(&windows[i]);
        sync_display(); puts("READY"); fflush(stdout);
        while (wl_display_dispatch(display)>=0) {}
        CHECK(0 && "unexpected compositor disconnect");
    }
    watch_motion=argc > 2 && !strcmp(argv[2], "hold");
    struct window window; create_window(&window);
    sync_display();
    if (argc > 2 && !strcmp(argv[2], "locked-window")) CHECK(keyboard_focus != window.surface);
    else CHECK(keyboard_focus == window.surface);
    if (argc > 2 && !strcmp(argv[2], "locked-hold")) {
        struct zwp_locked_pointer_v1 *lock=zwp_pointer_constraints_v1_lock_pointer(pointer_constraints,window.surface,pointer,NULL,ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_PERSISTENT);
        zwp_locked_pointer_v1_add_listener(lock,&test_lock_listener,NULL); sync_display();
    }
    if (argc > 2 && !strcmp(argv[2], "confined-hold")) {
        struct zwp_confined_pointer_v1 *confine=zwp_pointer_constraints_v1_confine_pointer(pointer_constraints,window.surface,pointer,NULL,ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_PERSISTENT);
        zwp_confined_pointer_v1_add_listener(confine,&test_confine_listener,NULL); sync_display();
    }
    if (argc > 2 && (!strcmp(argv[2], "hold") || !strcmp(argv[2], "quiet-hold") || !strcmp(argv[2], "locked-hold") || !strcmp(argv[2], "confined-hold") || !strcmp(argv[2], "owner") || !strcmp(argv[2], "locked-window"))) {
        struct popup owned_popup;
        if (!strcmp(argv[2], "owner")) { create_popup(&owned_popup,&window); own_protocol_state(&window); }
        puts("READY"); fflush(stdout);
        while (wl_display_dispatch(display) >= 0) {}
        CHECK(0 && "unexpected compositor disconnect");
    }
    // A second real toplevel must retile the first and take keyboard focus. Destroying it
    // restores the first window's size and focus without reconnecting the surviving client.
    int original_width=window.width;
    struct window aborted; create_window_mode(&aborted,1); sync_display();
    CHECK(window.width == original_width && keyboard_focus == window.surface);
    xdg_toplevel_destroy(aborted.top); xdg_surface_destroy(aborted.xdg); wl_surface_destroy(aborted.surface);
    sync_display(); sync_display();
    CHECK(window.width == original_width && keyboard_focus == window.surface);
    struct ext_image_copy_capture_session_v1 *pending_capture=new_session();
    struct pixels before=capture(pending_capture,0,0);
    size_t covered=color_count(&before,0xffff0000); free_pixels(&before);
    // Complete configure but deliberately supply no image: existing windows must
    // retain their size, focus and visible pixels for arbitrarily slow clients.
    struct window second; create_window_mode(&second,1); sync_display();
    usleep(150000); sync_display();
    CHECK(window.width == original_width && keyboard_focus == window.surface);
    struct pixels waiting=capture(pending_capture,0,0);
    CHECK(color_count(&waiting,0xffff0000) == covered); free_pixels(&waiting);
    ext_image_copy_capture_session_v1_destroy(pending_capture);
    second.defer_buffer=0; paint_window(&second); sync_display(); sync_display();
    CHECK(window.width < original_width && keyboard_focus == second.surface);
    // Delay the survivor's larger replacement image after closing a stack
    // window. Its old buffer must cover the expanded tile instead of exposing
    // the wallpaper while the client processes configure.
    window.defer_buffer=1;
    second.defer_buffer=1;
    wl_surface_attach(second.surface,NULL,0,0); wl_surface_commit(second.surface);
    sync_display(); sync_display();
    CHECK(window.width == original_width && keyboard_focus == window.surface);
    // An ack plus an empty commit is not a replacement image.
    wl_surface_commit(window.surface); sync_display();
    struct ext_image_copy_capture_session_v1 *close_capture=new_session();
    struct pixels closing=capture(close_capture,0,0);
    CHECK(closing.map[(closing.height/2)*closing.width+closing.width-64] == 0xffff0000);
    free_pixels(&closing);
    ext_image_copy_capture_session_v1_destroy(close_capture);
    window.defer_buffer=0; paint_window(&window); sync_display(); sync_display();
    xdg_toplevel_destroy(second.top); xdg_surface_destroy(second.xdg); wl_surface_destroy(second.surface);
    sync_display(); sync_display(); CHECK(window.width == original_width && keyboard_focus == window.surface);
    if (second.has_pixels) free_pixels(&second.pixels);
    // A client may destroy its toplevel directly without a preceding null
    // buffer. The same cover must work for that close path as well.
    struct window destroyed; create_window(&destroyed); sync_display();
    window.defer_buffer=1;
    xdg_toplevel_destroy(destroyed.top); xdg_surface_destroy(destroyed.xdg); wl_surface_destroy(destroyed.surface);
    sync_display(); sync_display();
    close_capture=new_session(); closing=capture(close_capture,0,0);
    CHECK(closing.map[(closing.height/2)*closing.width+closing.width-64] == 0xffff0000);
    free_pixels(&closing); ext_image_copy_capture_session_v1_destroy(close_capture);
    window.defer_buffer=0; paint_window(&window); sync_display(); sync_display();
    if (destroyed.has_pixels) free_pixels(&destroyed.pixels);
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
