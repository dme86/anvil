#define _GNU_SOURCE
#include <wayland-client.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include "primary-selection-client.h"
#include "ext-data-control-client.h"
#include "wlr-data-control-client.h"

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "CLIPBOARD FAIL line %d: %s\n", __LINE__, #c); exit(1); } } while (0)
#define MIME "text/plain;charset=utf-8"
enum kind { APP, EXT, WLR };
enum target { CLIPBOARD, PRIMARY };
struct offer { enum kind kind; enum target target; void *object; int text; };
struct peer {
    enum kind kind;
    struct wl_display *display;
    struct wl_seat *seat;
    struct wl_data_device *app;
    struct zwp_primary_selection_device_v1 *primary;
    struct ext_data_control_manager_v1 *ext_manager;
    struct ext_data_control_device_v1 *ext;
    struct zwlr_data_control_manager_v1 *wlr_manager;
    struct zwlr_data_control_device_v1 *wlr;
    struct offer offers[64];
    size_t offer_count;
    struct offer *selected[2];
    unsigned changes[2];
};
struct source { const char *text; unsigned cancelled; };
static struct peer peers[3];

static double seconds(void) {
    struct timespec now; CHECK(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
    return now.tv_sec + now.tv_nsec / 1e9;
}
static void pump(void) {
    struct pollfd fds[3];
    for (int i = 0; i < 3; ++i) {
        CHECK(wl_display_dispatch_pending(peers[i].display) >= 0);
        CHECK(wl_display_flush(peers[i].display) >= 0 || errno == EAGAIN);
        fds[i] = (struct pollfd){wl_display_get_fd(peers[i].display), POLLIN, 0};
    }
    CHECK(poll(fds, 3, 10) >= 0);
    for (int i = 0; i < 3; ++i) {
        CHECK(!(fds[i].revents & (POLLERR | POLLHUP | POLLNVAL)));
        if (fds[i].revents & POLLIN) CHECK(wl_display_dispatch(peers[i].display) >= 0);
    }
}
static void changed(unsigned old[3][2], enum target target) {
    double deadline = seconds() + 3;
    for (;;) {
        int ready = 1;
        for (int i = 0; i < 3; ++i) ready &= peers[i].changes[target] > old[i][target];
        if (ready) return;
        CHECK(seconds() < deadline); pump();
    }
}
static void snapshot(unsigned old[3][2]) {
    for (int i = 0; i < 3; ++i)
        for (int j = 0; j < 2; ++j) old[i][j] = peers[i].changes[j];
}
static struct offer *new_offer(struct peer *peer, void *object, enum target target) {
    CHECK(peer->offer_count < 64);
    struct offer *offer = &peer->offers[peer->offer_count++];
    *offer = (struct offer){peer->kind, target, object, 0};
    wl_proxy_set_user_data(object, offer);
    return offer;
}
static void selected(struct peer *peer, void *object, enum target target) {
    peer->selected[target] = object ? wl_proxy_get_user_data(object) : NULL;
    peer->changes[target]++;
}
#define OFFER_MIME(prefix, type) \
    static void prefix##_mime(void *data, struct type *object, const char *mime) { \
        struct offer *offer = data; if (!strcmp(mime, MIME)) offer->text = 1; \
    }
OFFER_MIME(app, wl_data_offer)
OFFER_MIME(primary, zwp_primary_selection_offer_v1)
OFFER_MIME(ext, ext_data_control_offer_v1)
OFFER_MIME(wlr, zwlr_data_control_offer_v1)
static const struct wl_data_offer_listener app_offer_listener = {.offer=app_mime};
static const struct zwp_primary_selection_offer_v1_listener primary_offer_listener = {.offer=primary_mime};
static const struct ext_data_control_offer_v1_listener ext_offer_listener = {.offer=ext_mime};
static const struct zwlr_data_control_offer_v1_listener wlr_offer_listener = {.offer=wlr_mime};
static void app_offer(void *data, struct wl_data_device *device, struct wl_data_offer *object) {
    wl_data_offer_add_listener(object, &app_offer_listener, new_offer(data, object, CLIPBOARD));
}
static void primary_offer(void *data, struct zwp_primary_selection_device_v1 *device, struct zwp_primary_selection_offer_v1 *object) {
    zwp_primary_selection_offer_v1_add_listener(object, &primary_offer_listener, new_offer(data, object, PRIMARY));
}
static void ext_offer(void *data, struct ext_data_control_device_v1 *device, struct ext_data_control_offer_v1 *object) {
    ext_data_control_offer_v1_add_listener(object, &ext_offer_listener, new_offer(data, object, CLIPBOARD));
}
static void wlr_offer(void *data, struct zwlr_data_control_device_v1 *device, struct zwlr_data_control_offer_v1 *object) {
    zwlr_data_control_offer_v1_add_listener(object, &wlr_offer_listener, new_offer(data, object, CLIPBOARD));
}
#define SELECTION(prefix, device, offer) \
    static void prefix##_clipboard(void *data, struct device *d, struct offer *o) { selected(data, o, CLIPBOARD); } \
    static void prefix##_primary(void *data, struct device *d, struct offer *o) { selected(data, o, PRIMARY); }
static void app_clipboard(void *data, struct wl_data_device *device, struct wl_data_offer *object) { selected(data, object, CLIPBOARD); }
static void primary_primary(void *data, struct zwp_primary_selection_device_v1 *device, struct zwp_primary_selection_offer_v1 *object) { selected(data, object, PRIMARY); }
SELECTION(ext, ext_data_control_device_v1, ext_data_control_offer_v1)
SELECTION(wlr, zwlr_data_control_device_v1, zwlr_data_control_offer_v1)
static void ext_finished(void *data, struct ext_data_control_device_v1 *device) { CHECK(0 && "ext device finished unexpectedly"); }
static void wlr_finished(void *data, struct zwlr_data_control_device_v1 *device) { CHECK(0 && "wlr device finished unexpectedly"); }
static const struct wl_data_device_listener app_listener = {.data_offer=app_offer, .selection=app_clipboard};
static const struct zwp_primary_selection_device_v1_listener primary_listener = {.data_offer=primary_offer, .selection=primary_primary};
static const struct ext_data_control_device_v1_listener ext_listener = {.data_offer=ext_offer, .selection=ext_clipboard, .primary_selection=ext_primary, .finished=ext_finished};
static const struct zwlr_data_control_device_v1_listener wlr_listener = {.data_offer=wlr_offer, .selection=wlr_clipboard, .primary_selection=wlr_primary, .finished=wlr_finished};

static void global(void *data, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version) {
    struct peer *peer = data;
    if (!strcmp(interface, "wl_seat")) peer->seat = wl_registry_bind(registry, name, &wl_seat_interface, 1);
    if (peer->kind == EXT && !strcmp(interface, "ext_data_control_manager_v1"))
        peer->ext_manager = wl_registry_bind(registry, name, &ext_data_control_manager_v1_interface, 1);
    if (peer->kind == WLR && !strcmp(interface, "zwlr_data_control_manager_v1")) {
        CHECK(version >= 2);
        peer->wlr_manager = wl_registry_bind(registry, name, &zwlr_data_control_manager_v1_interface, 2);
    }
}
static void global_remove(void *data, struct wl_registry *registry, uint32_t name) {}
static const struct wl_registry_listener registry_listener = {.global=global, .global_remove=global_remove};
static void connect_agent(struct peer *peer, enum kind kind) {
    peer->kind = kind;
    peer->display = wl_display_connect(NULL); CHECK(peer->display);
    struct wl_registry *registry = wl_display_get_registry(peer->display);
    wl_registry_add_listener(registry, &registry_listener, peer);
    CHECK(wl_display_roundtrip(peer->display) >= 0);
    CHECK(peer->seat);
    // The agent connections create no surface and never hold keyboard focus.
    if (kind == EXT) {
        CHECK(peer->ext_manager);
        peer->ext = ext_data_control_manager_v1_get_data_device(peer->ext_manager, peer->seat);
        ext_data_control_device_v1_add_listener(peer->ext, &ext_listener, peer);
    } else {
        CHECK(peer->wlr_manager);
        peer->wlr = zwlr_data_control_manager_v1_get_data_device(peer->wlr_manager, peer->seat);
        zwlr_data_control_device_v1_add_listener(peer->wlr, &wlr_listener, peer);
    }
    CHECK(wl_display_roundtrip(peer->display) >= 0);
    wl_registry_destroy(registry);
}
static void send_text(struct source *source, int fd) {
    size_t length = strlen(source->text);
    CHECK(write(fd, source->text, length) == (ssize_t)length); close(fd);
}
#define SOURCE(prefix, type) \
    static void prefix##_send(void *data, struct type *object, const char *mime, int32_t fd) { CHECK(!strcmp(mime, MIME)); send_text(data, fd); } \
    static void prefix##_cancelled(void *data, struct type *object) { ((struct source *)data)->cancelled++; }
SOURCE(app, wl_data_source)
SOURCE(primary, zwp_primary_selection_source_v1)
SOURCE(ext, ext_data_control_source_v1)
SOURCE(wlr, zwlr_data_control_source_v1)
static const struct wl_data_source_listener app_source_listener = {.send=app_send, .cancelled=app_cancelled};
static const struct zwp_primary_selection_source_v1_listener primary_source_listener = {.send=primary_send, .cancelled=primary_cancelled};
static const struct ext_data_control_source_v1_listener ext_source_listener = {.send=ext_send, .cancelled=ext_cancelled};
static const struct zwlr_data_control_source_v1_listener wlr_source_listener = {.send=wlr_send, .cancelled=wlr_cancelled};

static void expect_text(struct peer *peer, enum target target, const char *expected) {
    struct offer *offer = peer->selected[target]; CHECK(offer && offer->text);
    int fds[2]; CHECK(pipe2(fds, O_CLOEXEC | O_NONBLOCK) == 0);
    if (peer->kind == EXT) ext_data_control_offer_v1_receive(offer->object, MIME, fds[1]);
    else if (peer->kind == WLR) zwlr_data_control_offer_v1_receive(offer->object, MIME, fds[1]);
    else if (target == PRIMARY) zwp_primary_selection_offer_v1_receive(offer->object, MIME, fds[1]);
    else wl_data_offer_receive(offer->object, MIME, fds[1]);
    close(fds[1]);
    char text[256]; size_t length = 0; double deadline = seconds() + 3;
    for (;;) {
        ssize_t count = read(fds[0], text + length, sizeof(text) - length);
        if (count > 0) { length += (size_t)count; CHECK(length < sizeof(text)); }
        else if (count == 0) break;
        else CHECK(errno == EAGAIN || errno == EINTR);
        CHECK(seconds() < deadline); pump();
    }
    close(fds[0]);
    CHECK(length == strlen(expected) && !memcmp(text, expected, length));
}
static void expect_all(enum target target, const char *text) {
    for (int i = 0; i < 3; ++i) expect_text(&peers[i], target, text);
}

void test_clipboard(struct wl_display *display, struct wl_seat *seat,
                    struct wl_data_device_manager *manager,
                    struct zwp_primary_selection_device_manager_v1 *primary_manager,
                    uint32_t serial) {
    CHECK(serial && manager && primary_manager);
    peers[0].kind = APP; peers[0].display = display; peers[0].seat = seat;
    peers[0].app = wl_data_device_manager_get_data_device(manager, seat);
    wl_data_device_add_listener(peers[0].app, &app_listener, &peers[0]);
    peers[0].primary = zwp_primary_selection_device_manager_v1_get_device(primary_manager, seat);
    zwp_primary_selection_device_v1_add_listener(peers[0].primary, &primary_listener, &peers[0]);
    CHECK(wl_display_roundtrip(display) >= 0);
    connect_agent(&peers[1], EXT); connect_agent(&peers[2], WLR);
    struct source values[] = {{"app-copy",0},{"app-primary",0},{"ext-copy",0},{"wlr-primary",0},{"wlr-copy",0},{"ext-primary",0}};
    unsigned old[3][2]; snapshot(old);
    struct wl_data_source *app = wl_data_device_manager_create_data_source(manager);
    wl_data_source_add_listener(app, &app_source_listener, &values[0]);
    wl_data_source_offer(app, MIME); wl_data_device_set_selection(peers[0].app, app, serial);
    struct zwp_primary_selection_source_v1 *primary = zwp_primary_selection_device_manager_v1_create_source(primary_manager);
    zwp_primary_selection_source_v1_add_listener(primary, &primary_source_listener, &values[1]);
    zwp_primary_selection_source_v1_offer(primary, MIME);
    zwp_primary_selection_device_v1_set_selection(peers[0].primary, primary, serial);
    changed(old, CLIPBOARD); changed(old, PRIMARY);
    expect_all(CLIPBOARD, "app-copy"); expect_all(PRIMARY, "app-primary");

    snapshot(old);
    struct ext_data_control_source_v1 *ext = ext_data_control_manager_v1_create_data_source(peers[1].ext_manager);
    ext_data_control_source_v1_add_listener(ext, &ext_source_listener, &values[2]);
    ext_data_control_source_v1_offer(ext, MIME);
    ext_data_control_device_v1_set_selection(peers[1].ext, ext);
    changed(old, CLIPBOARD); expect_all(CLIPBOARD, "ext-copy"); expect_all(PRIMARY, "app-primary"); CHECK(values[0].cancelled == 1);

    snapshot(old);
    struct zwlr_data_control_source_v1 *wlr_primary = zwlr_data_control_manager_v1_create_data_source(peers[2].wlr_manager);
    zwlr_data_control_source_v1_add_listener(wlr_primary, &wlr_source_listener, &values[3]);
    zwlr_data_control_source_v1_offer(wlr_primary, MIME);
    zwlr_data_control_device_v1_set_primary_selection(peers[2].wlr, wlr_primary);
    changed(old, PRIMARY); expect_all(PRIMARY, "wlr-primary"); expect_all(CLIPBOARD, "ext-copy"); CHECK(values[1].cancelled == 1);

    snapshot(old);
    struct zwlr_data_control_source_v1 *wlr = zwlr_data_control_manager_v1_create_data_source(peers[2].wlr_manager);
    zwlr_data_control_source_v1_add_listener(wlr, &wlr_source_listener, &values[4]);
    zwlr_data_control_source_v1_offer(wlr, MIME);
    zwlr_data_control_device_v1_set_selection(peers[2].wlr, wlr);
    changed(old, CLIPBOARD); expect_all(CLIPBOARD, "wlr-copy"); CHECK(values[2].cancelled == 1);

    snapshot(old);
    struct ext_data_control_source_v1 *ext_primary = ext_data_control_manager_v1_create_data_source(peers[1].ext_manager);
    ext_data_control_source_v1_add_listener(ext_primary, &ext_source_listener, &values[5]);
    ext_data_control_source_v1_offer(ext_primary, MIME);
    ext_data_control_device_v1_set_primary_selection(peers[1].ext, ext_primary);
    changed(old, PRIMARY); expect_all(PRIMARY, "ext-primary"); expect_all(CLIPBOARD, "wlr-copy"); CHECK(values[3].cancelled == 1);

    snapshot(old); ext_data_control_device_v1_set_selection(peers[1].ext, NULL);
    changed(old, CLIPBOARD);
    for (int i = 0; i < 3; ++i) CHECK(!peers[i].selected[CLIPBOARD]);
    expect_all(PRIMARY, "ext-primary"); CHECK(values[4].cancelled == 1);
    snapshot(old); zwlr_data_control_device_v1_set_primary_selection(peers[2].wlr, NULL);
    changed(old, PRIMARY);
    for (int i = 0; i < 3; ++i) CHECK(!peers[i].selected[PRIMARY]);
    CHECK(values[5].cancelled == 1);
    wl_display_disconnect(peers[1].display); wl_display_disconnect(peers[2].display);
    puts("PASS: focused app and unfocused ext/wlr agents exchange, replace and clear independent selections");
}
