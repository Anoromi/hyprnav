/*
 * hyprnav-capture - window identification and live JPEG capture for hyprnav.
 *
 * Two jobs, one process, because both need the same Wayland connection and the
 * same toplevel handles:
 *
 *  1. Pair Hyprland window addresses (`hyprctl clients` `address`) with
 *     ext-foreign-toplevel identifiers. ext-foreign-toplevel-list-v1 hands out
 *     the identifier, hyprland-toplevel-mapping-v1 turns a toplevel handle
 *     into the 64-bit address; neither alone is enough.
 *
 *  2. Stream JPEG frames of individual windows through
 *     ext-image-copy-capture-v1 with an
 *     ext_foreign_toplevel_image_capture_source_manager_v1 source. The
 *     compositor only completes a capture once the window has new damage, so a
 *     static window costs nothing: no frame, no encode, no wakeup.
 *
 * Modes:
 *   (no args)        long-lived: NDJSON on stdout, NDJSON commands on stdin
 *   --list, --once   print the current `add` lines and exit
 *   --resolve 0xADDR print the bare identifier, or exit 3 when not found
 *
 * It is installed twice: as `hyprnav-capture` and as `hyprnav-toplevel-map`
 * (a symlink), the latter being the name for the identification half alone.
 *
 * stdout, one JSON object per line, flushed per line:
 *   {"ev":"add","addr":"0x…","id":"18000004","app":"…","title":"…"}
 *   {"ev":"title","addr":"0x…","title":"…"}
 *   {"ev":"close","addr":"0x…"}
 *   {"ev":"ready"}
 *   {"ev":"capture_failed","addr":"0x…","reason":"…"}
 *   {"ev":"frame","addr":"0x…","len":N,"w":W,"h":H,"enc_ms":1.23}
 * A `frame` line is followed immediately by exactly `len` raw JPEG bytes and
 * then the next line; nothing else may be interleaved.
 *
 * stdin, one JSON object per line:
 *   {"op":"start","addr":"0x…","max_width":640,"quality":60,"max_fps":8}
 *   {"op":"stop","addr":"0x…"}
 * A second `start` for a running address just updates its parameters; the
 * caller does the refcounting.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include <turbojpeg.h>

#include "ext-foreign-toplevel-list-v1-client-protocol.h"
#include "hyprland-toplevel-mapping-v1-client-protocol.h"
#include "ext-image-capture-source-v1-client-protocol.h"
#include "ext-image-copy-capture-v1-client-protocol.h"

enum mode { MODE_STREAM, MODE_LIST, MODE_RESOLVE };

#define DEFAULT_MAX_WIDTH 640
#define DEFAULT_QUALITY 60
#define DEFAULT_MAX_FPS 8
/* Unexplained capture failures in a row before the address is given up on. */
#define FAILURES_BEFORE_GIVING_UP 3

struct toplevel;

/* Everything needed to keep one window's capture running. */
struct capture {
	struct toplevel *owner;
	struct ext_image_capture_source_v1 *source;
	struct ext_image_copy_capture_session_v1 *session;
	struct ext_image_copy_capture_frame_v1 *frame;

	/* Constraints staged by the current event batch, committed on `done`. */
	uint32_t pending_width, pending_height, pending_format;
	bool pending_has_size, pending_has_format;
	/* Constraints in force. */
	uint32_t width, height, format;
	bool configured;

	/* The shm buffer handed to the compositor. */
	struct wl_buffer *buffer;
	uint8_t *pixels;
	size_t pixels_size;
	uint32_t buffer_width, buffer_height, buffer_stride, buffer_format;
	/* Copy of the last captured pixels, to skip re-encoding a still image. */
	uint8_t *previous;
	size_t previous_size;

	int max_width, quality, max_fps;
	bool frame_in_flight;
	uint32_t failures;
	/* Earliest ms at which the next capture may be requested. */
	uint64_t next_capture_ms;
};

struct toplevel {
	struct toplevel *next;
	struct ext_foreign_toplevel_handle_v1 *handle;
	char *identifier;
	char *app_id;
	char *title;
	char *next_title;
	uint64_t address;
	bool has_address;
	bool announced;
	bool gone;
	struct capture *capture;
};

static struct wl_display *display;
static struct wl_shm *shm;
static struct ext_foreign_toplevel_list_v1 *toplevel_list;
static struct hyprland_toplevel_mapping_manager_v1 *mapping_manager;
static struct ext_foreign_toplevel_image_capture_source_manager_v1 *source_manager;
static struct ext_image_copy_capture_manager_v1 *copy_manager;

static struct toplevel *toplevels;
static enum mode run_mode = MODE_STREAM;
static uint64_t wanted_address;
static volatile sig_atomic_t stop_requested;

static tjhandle jpeg;
static unsigned char *jpeg_buffer;
static size_t jpeg_buffer_size;
static uint8_t *scale_buffer;
static size_t scale_buffer_size;

/* Pending start commands whose window has not appeared yet are simply
 * rejected; the daemon re-issues them when it sees the `add`. */

static void on_signal(int signo) {
	(void)signo;
	stop_requested = 1;
}

static uint64_t now_ms(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000u + (uint64_t)ts.tv_nsec / 1000000u;
}

static void *xalloc(size_t size) {
	void *memory = calloc(1, size);
	if (!memory) {
		fprintf(stderr, "hyprnav-capture: out of memory\n");
		exit(1);
	}
	return memory;
}

static char *dup_string(const char *value) {
	char *copy = strdup(value ? value : "");
	if (!copy) {
		fprintf(stderr, "hyprnav-capture: out of memory\n");
		exit(1);
	}
	return copy;
}

static void print_json_string(const char *value) {
	for (const unsigned char *p = (const unsigned char *)(value ? value : ""); *p; p++) {
		switch (*p) {
		case '"': fputs("\\\"", stdout); break;
		case '\\': fputs("\\\\", stdout); break;
		case '\n': fputs("\\n", stdout); break;
		case '\r': fputs("\\r", stdout); break;
		case '\t': fputs("\\t", stdout); break;
		default:
			if (*p < 0x20) {
				printf("\\u%04x", *p);
			} else {
				fputc(*p, stdout);
			}
		}
	}
}

/* Lowercase 0x + hex without leading zeros, exactly like `hyprctl clients`. */
static void format_address(char *buffer, size_t size, uint64_t address) {
	snprintf(buffer, size, "0x%" PRIx64, address);
}

static void emit_add(const struct toplevel *entry) {
	char address[32];
	format_address(address, sizeof(address), entry->address);
	printf("{\"ev\":\"add\",\"addr\":\"%s\",\"id\":\"", address);
	print_json_string(entry->identifier);
	fputs("\",\"app\":\"", stdout);
	print_json_string(entry->app_id);
	fputs("\",\"title\":\"", stdout);
	print_json_string(entry->title);
	fputs("\"}\n", stdout);
	fflush(stdout);
}

static void emit_title(const struct toplevel *entry) {
	char address[32];
	format_address(address, sizeof(address), entry->address);
	printf("{\"ev\":\"title\",\"addr\":\"%s\",\"title\":\"", address);
	print_json_string(entry->title);
	fputs("\"}\n", stdout);
	fflush(stdout);
}

static void emit_close(const struct toplevel *entry) {
	char address[32];
	format_address(address, sizeof(address), entry->address);
	printf("{\"ev\":\"close\",\"addr\":\"%s\"}\n", address);
	fflush(stdout);
}

static void emit_capture_failed(uint64_t raw_address, const char *reason) {
	char address[32];
	format_address(address, sizeof(address), raw_address);
	printf("{\"ev\":\"capture_failed\",\"addr\":\"%s\",\"reason\":\"", address);
	print_json_string(reason);
	fputs("\"}\n", stdout);
	fflush(stdout);
}

static void emit_frame(uint64_t raw_address, const unsigned char *data, size_t length,
                       uint32_t width, uint32_t height, double encode_ms) {
	char address[32];
	format_address(address, sizeof(address), raw_address);
	printf("{\"ev\":\"frame\",\"addr\":\"%s\",\"len\":%zu,\"w\":%u,\"h\":%u,\"enc_ms\":%.2f}\n",
	       address, length, width, height, encode_ms);
	fwrite(data, 1, length, stdout);
	fflush(stdout);
}

// ---------------------------------------------------------------------------
// tiny JSON field readers, enough for the commands this program accepts
// ---------------------------------------------------------------------------

/* Value of "key" as a string, copied into `out`. */
static bool json_string_field(const char *line, const char *key, char *out, size_t size) {
	char needle[64];
	snprintf(needle, sizeof(needle), "\"%s\"", key);
	const char *at = strstr(line, needle);
	if (!at) {
		return false;
	}
	at = strchr(at + strlen(needle), ':');
	if (!at) {
		return false;
	}
	at++;
	while (*at == ' ' || *at == '\t') {
		at++;
	}
	if (*at != '"') {
		return false;
	}
	at++;
	size_t written = 0;
	while (*at && *at != '"' && written + 1 < size) {
		if (*at == '\\' && at[1]) {
			at++;
		}
		out[written++] = *at++;
	}
	out[written] = '\0';
	return true;
}

static bool json_int_field(const char *line, const char *key, long *out) {
	char needle[64];
	snprintf(needle, sizeof(needle), "\"%s\"", key);
	const char *at = strstr(line, needle);
	if (!at) {
		return false;
	}
	at = strchr(at + strlen(needle), ':');
	if (!at) {
		return false;
	}
	at++;
	while (*at == ' ' || *at == '\t') {
		at++;
	}
	char *end = NULL;
	long value = strtol(at, &end, 10);
	if (end == at) {
		return false;
	}
	*out = value;
	return true;
}

static bool parse_address(const char *text, uint64_t *out) {
	if (!text) {
		return false;
	}
	if (strncmp(text, "address:", 8) == 0) {
		text += 8;
	}
	errno = 0;
	char *end = NULL;
	uint64_t value = strtoull(text, &end, 0);
	if (errno != 0 || !end || *end != '\0' || end == text) {
		return false;
	}
	*out = value;
	return true;
}

static struct toplevel *find_by_address(uint64_t address) {
	for (struct toplevel *entry = toplevels; entry; entry = entry->next) {
		if (entry->has_address && !entry->gone && entry->address == address) {
			return entry;
		}
	}
	return NULL;
}

// ---------------------------------------------------------------------------
// shm buffers
// ---------------------------------------------------------------------------

static void capture_release_buffer(struct capture *capture) {
	if (capture->buffer) {
		wl_buffer_destroy(capture->buffer);
		capture->buffer = NULL;
	}
	if (capture->pixels) {
		munmap(capture->pixels, capture->pixels_size);
		capture->pixels = NULL;
		capture->pixels_size = 0;
	}
	free(capture->previous);
	capture->previous = NULL;
	capture->previous_size = 0;
	capture->buffer_width = capture->buffer_height = 0;
}

/* (Re)create the shm buffer the compositor copies into. */
static bool capture_ensure_buffer(struct capture *capture) {
	if (capture->buffer && capture->buffer_width == capture->width &&
	    capture->buffer_height == capture->height && capture->buffer_format == capture->format) {
		return true;
	}
	capture_release_buffer(capture);

	uint32_t stride = capture->width * 4;
	size_t size = (size_t)stride * capture->height;
	if (size == 0) {
		return false;
	}
	int fd = memfd_create("hyprnav-capture", MFD_CLOEXEC);
	if (fd < 0) {
		return false;
	}
	if (ftruncate(fd, (off_t)size) != 0) {
		close(fd);
		return false;
	}
	void *pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (pixels == MAP_FAILED) {
		close(fd);
		return false;
	}
	struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int32_t)size);
	close(fd);
	if (!pool) {
		munmap(pixels, size);
		return false;
	}
	capture->buffer = wl_shm_pool_create_buffer(pool, 0, (int32_t)capture->width,
	                                            (int32_t)capture->height, (int32_t)stride,
	                                            capture->format);
	wl_shm_pool_destroy(pool);
	if (!capture->buffer) {
		munmap(pixels, size);
		return false;
	}
	capture->pixels = pixels;
	capture->pixels_size = size;
	capture->buffer_width = capture->width;
	capture->buffer_height = capture->height;
	capture->buffer_stride = stride;
	capture->buffer_format = capture->format;
	return true;
}

// ---------------------------------------------------------------------------
// downscale + encode
// ---------------------------------------------------------------------------

/* Box-average `src` (BGRx, `stride` bytes per row) into packed BGR of
 * `dst_width` x `dst_height`. Integer maths only; no floats, no libraries. */
static void box_downscale(const uint8_t *src, uint32_t stride, uint32_t src_width,
                          uint32_t src_height, uint8_t *dst, uint32_t dst_width,
                          uint32_t dst_height) {
	for (uint32_t y = 0; y < dst_height; y++) {
		uint32_t y0 = (uint32_t)((uint64_t)y * src_height / dst_height);
		uint32_t y1 = (uint32_t)((uint64_t)(y + 1) * src_height / dst_height);
		if (y1 <= y0) {
			y1 = y0 + 1;
		}
		for (uint32_t x = 0; x < dst_width; x++) {
			uint32_t x0 = (uint32_t)((uint64_t)x * src_width / dst_width);
			uint32_t x1 = (uint32_t)((uint64_t)(x + 1) * src_width / dst_width);
			if (x1 <= x0) {
				x1 = x0 + 1;
			}
			uint32_t blue = 0, green = 0, red = 0, count = 0;
			for (uint32_t sy = y0; sy < y1; sy++) {
				const uint8_t *row = src + (size_t)sy * stride + (size_t)x0 * 4;
				for (uint32_t sx = x0; sx < x1; sx++) {
					blue += row[0];
					green += row[1];
					red += row[2];
					row += 4;
					count++;
				}
			}
			uint8_t *out = dst + ((size_t)y * dst_width + x) * 3;
			out[0] = (uint8_t)(blue / count);
			out[1] = (uint8_t)(green / count);
			out[2] = (uint8_t)(red / count);
		}
	}
}

static uint8_t *ensure_scale_buffer(size_t size) {
	if (scale_buffer_size >= size) {
		return scale_buffer;
	}
	free(scale_buffer);
	scale_buffer = xalloc(size);
	scale_buffer_size = size;
	return scale_buffer;
}

/* True when the capture holds exactly the pixels it held last time.
 *
 * Hyprland completes a frame whenever it repaints the window, which is not
 * always an actual visual change (a blinking cursor redraws the whole
 * surface, for instance). Comparing the raw buffer costs a fraction of a
 * JPEG encode, so a genuinely still window ends up costing almost nothing.
 */
static bool capture_is_unchanged(struct capture *capture) {
	size_t size = (size_t)capture->buffer_stride * capture->buffer_height;
	if (capture->previous_size == size && capture->previous &&
	    memcmp(capture->previous, capture->pixels, size) == 0) {
		return true;
	}
	if (capture->previous_size != size) {
		free(capture->previous);
		capture->previous = malloc(size);
		capture->previous_size = capture->previous ? size : 0;
	}
	if (capture->previous) {
		memcpy(capture->previous, capture->pixels, size);
	}
	return false;
}

/* Downscale if asked for, JPEG-encode, and emit the frame record. */
static void encode_and_emit(struct capture *capture) {
	struct timespec begin;
	clock_gettime(CLOCK_MONOTONIC, &begin);

	uint32_t source_width = capture->buffer_width;
	uint32_t source_height = capture->buffer_height;
	uint32_t target_width = source_width;
	uint32_t target_height = source_height;
	if (capture->max_width > 0 && source_width > (uint32_t)capture->max_width) {
		target_width = (uint32_t)capture->max_width;
		target_height = (uint32_t)(((uint64_t)source_height * target_width) / source_width);
		if (target_height == 0) {
			target_height = 1;
		}
	}

	const unsigned char *pixels;
	int pixel_format;
	int pitch;
	if (target_width == source_width && target_height == source_height) {
		pixels = capture->pixels;
		pixel_format = TJPF_BGRX;
		pitch = (int)capture->buffer_stride;
	} else {
		uint8_t *scaled = ensure_scale_buffer((size_t)target_width * target_height * 3);
		box_downscale(capture->pixels, capture->buffer_stride, source_width, source_height,
		              scaled, target_width, target_height);
		pixels = scaled;
		pixel_format = TJPF_BGR;
		pitch = (int)target_width * 3;
	}

	tj3Set(jpeg, TJPARAM_QUALITY, capture->quality);
	tj3Set(jpeg, TJPARAM_SUBSAMP, TJSAMP_420);
	size_t length = jpeg_buffer_size;
	if (tj3Compress8(jpeg, pixels, (int)target_width, pitch, (int)target_height, pixel_format,
	                 &jpeg_buffer, &length) != 0) {
		emit_capture_failed(capture->owner->address, "encode_failed");
		return;
	}
	if (length > jpeg_buffer_size) {
		jpeg_buffer_size = length;
	}

	struct timespec end;
	clock_gettime(CLOCK_MONOTONIC, &end);
	double encode_ms = (double)(end.tv_sec - begin.tv_sec) * 1000.0 +
	                   (double)(end.tv_nsec - begin.tv_nsec) / 1000000.0;
	emit_frame(capture->owner->address, jpeg_buffer, length, target_width, target_height,
	           encode_ms);
}

// ---------------------------------------------------------------------------
// capture session
// ---------------------------------------------------------------------------

static void capture_stop(struct toplevel *entry, const char *reason);
static void capture_request_frame(struct capture *capture);

static void frame_handle_transform(void *data, struct ext_image_copy_capture_frame_v1 *frame,
                                   uint32_t transform) {
	(void)data;
	(void)frame;
	(void)transform;
}

static void frame_handle_damage(void *data, struct ext_image_copy_capture_frame_v1 *frame,
                                int32_t x, int32_t y, int32_t width, int32_t height) {
	(void)data;
	(void)frame;
	(void)x;
	(void)y;
	(void)width;
	(void)height;
}

static void frame_handle_presentation_time(void *data,
                                           struct ext_image_copy_capture_frame_v1 *frame,
                                           uint32_t tv_sec_hi, uint32_t tv_sec_lo,
                                           uint32_t tv_nsec) {
	(void)data;
	(void)frame;
	(void)tv_sec_hi;
	(void)tv_sec_lo;
	(void)tv_nsec;
}

static void frame_finish(struct capture *capture) {
	if (capture->frame) {
		ext_image_copy_capture_frame_v1_destroy(capture->frame);
		capture->frame = NULL;
	}
	capture->frame_in_flight = false;
}

static void frame_handle_ready(void *data, struct ext_image_copy_capture_frame_v1 *frame) {
	(void)frame;
	struct capture *capture = data;
	capture->failures = 0;
	frame_finish(capture);
	if (!capture_is_unchanged(capture)) {
		encode_and_emit(capture);
	}
	uint64_t interval = capture->max_fps > 0 ? 1000u / (uint64_t)capture->max_fps : 0;
	capture->next_capture_ms = now_ms() + interval;
	/* Ask for the next one straight away: the compositor holds it until the
	 * window actually changes, which is what makes a static window free. */
	capture_request_frame(capture);
}

static void frame_handle_failed(void *data, struct ext_image_copy_capture_frame_v1 *frame,
                                uint32_t reason) {
	(void)frame;
	struct capture *capture = data;
	frame_finish(capture);
	switch (reason) {
	case EXT_IMAGE_COPY_CAPTURE_FRAME_V1_FAILURE_REASON_BUFFER_CONSTRAINTS:
		/* New constraints are on their way; wait for the next `done`. */
		capture->configured = false;
		return;
	case EXT_IMAGE_COPY_CAPTURE_FRAME_V1_FAILURE_REASON_STOPPED:
		capture_stop(capture->owner, "stopped");
		return;
	default:
		capture->failures++;
		if (capture->failures >= FAILURES_BEFORE_GIVING_UP) {
			capture_stop(capture->owner, "capture_failed");
			return;
		}
		capture_request_frame(capture);
	}
}

static const struct ext_image_copy_capture_frame_v1_listener frame_listener = {
	.transform = frame_handle_transform,
	.damage = frame_handle_damage,
	.presentation_time = frame_handle_presentation_time,
	.ready = frame_handle_ready,
	.failed = frame_handle_failed,
};

/* Start one capture if the session is configured and the rate limit allows. */
static void capture_request_frame(struct capture *capture) {
	if (!capture->session || capture->frame_in_flight || !capture->configured) {
		return;
	}
	if (now_ms() < capture->next_capture_ms) {
		return; /* the poll timeout brings us back */
	}
	if (!capture_ensure_buffer(capture)) {
		capture_stop(capture->owner, "buffer_failed");
		return;
	}
	capture->frame = ext_image_copy_capture_session_v1_create_frame(capture->session);
	if (!capture->frame) {
		capture_stop(capture->owner, "frame_failed");
		return;
	}
	ext_image_copy_capture_frame_v1_add_listener(capture->frame, &frame_listener, capture);
	ext_image_copy_capture_frame_v1_attach_buffer(capture->frame, capture->buffer);
	ext_image_copy_capture_frame_v1_damage_buffer(capture->frame, 0, 0,
	                                              (int32_t)capture->width,
	                                              (int32_t)capture->height);
	ext_image_copy_capture_frame_v1_capture(capture->frame);
	capture->frame_in_flight = true;
}

static void session_handle_buffer_size(void *data,
                                       struct ext_image_copy_capture_session_v1 *session,
                                       uint32_t width, uint32_t height) {
	(void)session;
	struct capture *capture = data;
	capture->pending_width = width;
	capture->pending_height = height;
	capture->pending_has_size = true;
}

static void session_handle_shm_format(void *data,
                                      struct ext_image_copy_capture_session_v1 *session,
                                      uint32_t format) {
	(void)session;
	struct capture *capture = data;
	/* XRGB is preferred; ARGB is accepted. Both are BGRx in memory. */
	if (!capture->pending_has_format || format == WL_SHM_FORMAT_XRGB8888) {
		if (format == WL_SHM_FORMAT_XRGB8888 || format == WL_SHM_FORMAT_ARGB8888) {
			capture->pending_format = format;
			capture->pending_has_format = true;
		}
	}
}

static void session_handle_dmabuf_device(void *data,
                                         struct ext_image_copy_capture_session_v1 *session,
                                         struct wl_array *device) {
	(void)data;
	(void)session;
	(void)device;
}

static void session_handle_dmabuf_format(void *data,
                                         struct ext_image_copy_capture_session_v1 *session,
                                         uint32_t format, struct wl_array *modifiers) {
	(void)data;
	(void)session;
	(void)format;
	(void)modifiers;
}

static void session_handle_done(void *data, struct ext_image_copy_capture_session_v1 *session) {
	(void)session;
	struct capture *capture = data;
	if (!capture->pending_has_size || !capture->pending_has_format) {
		emit_capture_failed(capture->owner->address, "no_shm_format");
		capture_stop(capture->owner, NULL);
		return;
	}
	capture->width = capture->pending_width;
	capture->height = capture->pending_height;
	capture->format = capture->pending_format;
	capture->pending_has_size = false;
	capture->pending_has_format = false;
	capture->configured = true;
	capture_request_frame(capture);
}

static void session_handle_stopped(void *data,
                                   struct ext_image_copy_capture_session_v1 *session) {
	(void)session;
	struct capture *capture = data;
	capture_stop(capture->owner, "stopped");
}

static const struct ext_image_copy_capture_session_v1_listener session_listener = {
	.buffer_size = session_handle_buffer_size,
	.shm_format = session_handle_shm_format,
	.dmabuf_device = session_handle_dmabuf_device,
	.dmabuf_format = session_handle_dmabuf_format,
	.done = session_handle_done,
	.stopped = session_handle_stopped,
};

/* Tear the capture down. `reason` non-NULL also reports it upstream. */
static void capture_stop(struct toplevel *entry, const char *reason) {
	struct capture *capture = entry->capture;
	if (!capture) {
		return;
	}
	entry->capture = NULL;
	if (capture->frame) {
		ext_image_copy_capture_frame_v1_destroy(capture->frame);
	}
	if (capture->session) {
		ext_image_copy_capture_session_v1_destroy(capture->session);
	}
	if (capture->source) {
		ext_image_capture_source_v1_destroy(capture->source);
	}
	capture_release_buffer(capture);
	uint64_t address = entry->address;
	free(capture);
	if (reason) {
		emit_capture_failed(address, reason);
	}
}

static void capture_start(struct toplevel *entry, int max_width, int quality, int max_fps) {
	if (entry->capture) {
		struct capture *capture = entry->capture;
		bool changed = capture->max_width != max_width || capture->quality != quality;
		capture->max_width = max_width;
		capture->quality = quality;
		capture->max_fps = max_fps;
		if (changed) {
			/* New geometry or quality: the cached comparison no longer
			 * describes what the caller would get, so re-emit. */
			free(capture->previous);
			capture->previous = NULL;
			capture->previous_size = 0;
		}
		return;
	}
	if (!source_manager || !copy_manager || !shm) {
		emit_capture_failed(entry->address, "unsupported");
		return;
	}
	struct capture *capture = xalloc(sizeof(*capture));
	capture->owner = entry;
	capture->max_width = max_width;
	capture->quality = quality;
	capture->max_fps = max_fps;
	capture->source = ext_foreign_toplevel_image_capture_source_manager_v1_create_source(
		source_manager, entry->handle);
	if (!capture->source) {
		free(capture);
		emit_capture_failed(entry->address, "no_source");
		return;
	}
	/* paint_cursors off: the pointer is not part of a window preview. */
	capture->session = ext_image_copy_capture_manager_v1_create_session(copy_manager,
	                                                                   capture->source, 0);
	if (!capture->session) {
		ext_image_capture_source_v1_destroy(capture->source);
		free(capture);
		emit_capture_failed(entry->address, "no_session");
		return;
	}
	ext_image_copy_capture_session_v1_add_listener(capture->session, &session_listener, capture);
	entry->capture = capture;
}

// ---------------------------------------------------------------------------
// toplevel bookkeeping
// ---------------------------------------------------------------------------

static void announce_if_ready(struct toplevel *entry) {
	if (entry->announced || !entry->has_address || !entry->identifier || !entry->title) {
		return;
	}
	entry->announced = true;
	if (run_mode == MODE_STREAM) {
		emit_add(entry);
	}
}

static void handle_closed(void *data, struct ext_foreign_toplevel_handle_v1 *handle) {
	(void)handle;
	struct toplevel *entry = data;
	entry->gone = true;
	capture_stop(entry, NULL);
	if (entry->announced && run_mode == MODE_STREAM) {
		emit_close(entry);
	}
	entry->announced = false;
}

static void handle_done(void *data, struct ext_foreign_toplevel_handle_v1 *handle) {
	(void)handle;
	struct toplevel *entry = data;
	bool title_changed = false;
	if (entry->next_title) {
		title_changed = !entry->title || strcmp(entry->title, entry->next_title) != 0;
		free(entry->title);
		entry->title = entry->next_title;
		entry->next_title = NULL;
	} else if (!entry->title) {
		entry->title = dup_string("");
	}
	if (entry->announced) {
		if (title_changed && run_mode == MODE_STREAM) {
			emit_title(entry);
		}
		return;
	}
	announce_if_ready(entry);
}

static void handle_title(void *data, struct ext_foreign_toplevel_handle_v1 *handle,
                         const char *title) {
	(void)handle;
	struct toplevel *entry = data;
	free(entry->next_title);
	entry->next_title = dup_string(title);
}

static void handle_app_id(void *data, struct ext_foreign_toplevel_handle_v1 *handle,
                          const char *app_id) {
	(void)handle;
	struct toplevel *entry = data;
	free(entry->app_id);
	entry->app_id = dup_string(app_id);
}

static void handle_identifier(void *data, struct ext_foreign_toplevel_handle_v1 *handle,
                              const char *identifier) {
	(void)handle;
	struct toplevel *entry = data;
	free(entry->identifier);
	entry->identifier = dup_string(identifier);
}

static const struct ext_foreign_toplevel_handle_v1_listener toplevel_listener = {
	.closed = handle_closed,
	.done = handle_done,
	.title = handle_title,
	.app_id = handle_app_id,
	.identifier = handle_identifier,
};

static void handle_window_address(void *data,
                                  struct hyprland_toplevel_window_mapping_handle_v1 *handle,
                                  uint32_t high, uint32_t low) {
	struct toplevel *entry = data;
	entry->address = ((uint64_t)high << 32) | (uint64_t)low;
	entry->has_address = true;
	hyprland_toplevel_window_mapping_handle_v1_destroy(handle);
	announce_if_ready(entry);
}

static void handle_window_failed(void *data,
                                 struct hyprland_toplevel_window_mapping_handle_v1 *handle) {
	(void)data;
	hyprland_toplevel_window_mapping_handle_v1_destroy(handle);
}

static const struct hyprland_toplevel_window_mapping_handle_v1_listener mapping_listener = {
	.window_address = handle_window_address,
	.failed = handle_window_failed,
};

static void handle_new_toplevel(void *data, struct ext_foreign_toplevel_list_v1 *list,
                                struct ext_foreign_toplevel_handle_v1 *handle) {
	(void)data;
	(void)list;
	struct toplevel *entry = xalloc(sizeof(*entry));
	entry->handle = handle;
	entry->next = toplevels;
	toplevels = entry;
	ext_foreign_toplevel_handle_v1_add_listener(handle, &toplevel_listener, entry);
	struct hyprland_toplevel_window_mapping_handle_v1 *mapping =
		hyprland_toplevel_mapping_manager_v1_get_window_for_toplevel(mapping_manager, handle);
	hyprland_toplevel_window_mapping_handle_v1_add_listener(mapping, &mapping_listener, entry);
}

static void handle_list_finished(void *data, struct ext_foreign_toplevel_list_v1 *list) {
	(void)data;
	(void)list;
}

static const struct ext_foreign_toplevel_list_v1_listener list_listener = {
	.toplevel = handle_new_toplevel,
	.finished = handle_list_finished,
};

static void handle_global(void *data, struct wl_registry *registry, uint32_t name,
                          const char *interface, uint32_t version) {
	(void)data;
	(void)version;
	if (strcmp(interface, ext_foreign_toplevel_list_v1_interface.name) == 0) {
		toplevel_list =
			wl_registry_bind(registry, name, &ext_foreign_toplevel_list_v1_interface, 1);
	} else if (strcmp(interface, hyprland_toplevel_mapping_manager_v1_interface.name) == 0) {
		mapping_manager = wl_registry_bind(
			registry, name, &hyprland_toplevel_mapping_manager_v1_interface, 1);
	} else if (strcmp(interface,
	                  ext_foreign_toplevel_image_capture_source_manager_v1_interface.name) == 0) {
		source_manager = wl_registry_bind(
			registry, name,
			&ext_foreign_toplevel_image_capture_source_manager_v1_interface, 1);
	} else if (strcmp(interface, ext_image_copy_capture_manager_v1_interface.name) == 0) {
		copy_manager = wl_registry_bind(registry, name,
		                                &ext_image_copy_capture_manager_v1_interface, 1);
	} else if (strcmp(interface, wl_shm_interface.name) == 0) {
		shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
	}
}

static void handle_global_remove(void *data, struct wl_registry *registry, uint32_t name) {
	(void)data;
	(void)registry;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = handle_global,
	.global_remove = handle_global_remove,
};

// ---------------------------------------------------------------------------
// stdin commands
// ---------------------------------------------------------------------------

static void run_command(const char *line) {
	char op[32];
	char address_text[64];
	if (!json_string_field(line, "op", op, sizeof(op))) {
		return;
	}
	if (!json_string_field(line, "addr", address_text, sizeof(address_text))) {
		return;
	}
	uint64_t address = 0;
	if (!parse_address(address_text, &address)) {
		return;
	}
	if (strcmp(op, "stop") == 0) {
		struct toplevel *entry = find_by_address(address);
		if (entry) {
			capture_stop(entry, NULL);
		}
		return;
	}
	if (strcmp(op, "start") != 0) {
		return;
	}
	long max_width = DEFAULT_MAX_WIDTH;
	long quality = DEFAULT_QUALITY;
	long max_fps = DEFAULT_MAX_FPS;
	json_int_field(line, "max_width", &max_width);
	json_int_field(line, "quality", &quality);
	json_int_field(line, "max_fps", &max_fps);
	if (max_width < 16) {
		max_width = 16;
	}
	if (quality < 1) {
		quality = 1;
	}
	if (quality > 100) {
		quality = 100;
	}
	if (max_fps < 1) {
		max_fps = 1;
	}
	if (max_fps > 60) {
		max_fps = 60;
	}
	struct toplevel *entry = find_by_address(address);
	if (!entry) {
		emit_capture_failed(address, "unknown_window");
		return;
	}
	capture_start(entry, (int)max_width, (int)quality, (int)max_fps);
}

/* Read whatever stdin has and run every complete line. Returns false on EOF. */
static bool drain_stdin(void) {
	static char buffer[8192];
	static size_t filled;
	ssize_t got = read(STDIN_FILENO, buffer + filled, sizeof(buffer) - filled - 1);
	if (got == 0) {
		return false;
	}
	if (got < 0) {
		return errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR;
	}
	filled += (size_t)got;
	buffer[filled] = '\0';
	char *start = buffer;
	char *newline;
	while ((newline = memchr(start, '\n', filled - (size_t)(start - buffer))) != NULL) {
		*newline = '\0';
		run_command(start);
		start = newline + 1;
	}
	size_t left = filled - (size_t)(start - buffer);
	memmove(buffer, start, left);
	filled = left;
	if (filled == sizeof(buffer) - 1) {
		filled = 0; /* an oversized line is dropped rather than wedging */
	}
	return true;
}

// ---------------------------------------------------------------------------

static int usage(FILE *out, int code) {
	fputs("usage: hyprnav-capture [--list | --once | --resolve 0xADDRESS]\n", out);
	return code;
}

/* How long the poll may sleep before a rate-limited capture becomes due. */
static int poll_timeout_ms(void) {
	int timeout = -1;
	uint64_t now = now_ms();
	for (struct toplevel *entry = toplevels; entry; entry = entry->next) {
		struct capture *capture = entry->capture;
		if (!capture || capture->frame_in_flight || !capture->configured) {
			continue;
		}
		if (capture->next_capture_ms <= now) {
			return 0;
		}
		int wait = (int)(capture->next_capture_ms - now);
		if (timeout < 0 || wait < timeout) {
			timeout = wait;
		}
	}
	return timeout;
}

static void service_rate_limited_captures(void) {
	for (struct toplevel *entry = toplevels; entry; entry = entry->next) {
		if (entry->capture) {
			capture_request_frame(entry->capture);
		}
	}
}

int main(int argc, char **argv) {
	if (argc > 1) {
		if ((strcmp(argv[1], "--once") == 0 || strcmp(argv[1], "--list") == 0) && argc == 2) {
			run_mode = MODE_LIST;
		} else if (strcmp(argv[1], "--resolve") == 0 && argc == 3) {
			run_mode = MODE_RESOLVE;
			if (!parse_address(argv[2], &wanted_address)) {
				fprintf(stderr, "hyprnav-capture: bad address %s\n", argv[2]);
				return 2;
			}
		} else if (strcmp(argv[1], "--help") == 0 || strcmp(argv[1], "-h") == 0) {
			return usage(stdout, 0);
		} else {
			return usage(stderr, 2);
		}
	}

	struct sigaction action = {0};
	action.sa_handler = on_signal;
	sigaction(SIGTERM, &action, NULL);
	sigaction(SIGINT, &action, NULL);
	signal(SIGPIPE, SIG_IGN);

	display = wl_display_connect(NULL);
	if (!display) {
		fprintf(stderr, "hyprnav-capture: cannot connect to the Wayland display\n");
		return 2;
	}
	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, NULL);
	wl_display_roundtrip(display);

	if (!toplevel_list) {
		fprintf(stderr,
		        "hyprnav-capture: compositor does not offer ext_foreign_toplevel_list_v1\n");
		return 2;
	}
	if (!mapping_manager) {
		fprintf(stderr, "hyprnav-capture: compositor does not offer "
		                "hyprland_toplevel_mapping_manager_v1\n");
		return 2;
	}
	if (run_mode == MODE_STREAM && (!source_manager || !copy_manager || !shm)) {
		/* Identification still works; only capture is unavailable. */
		fprintf(stderr, "hyprnav-capture: compositor does not offer "
		                "ext-image-copy-capture-v1; frames are unavailable\n");
	}

	ext_foreign_toplevel_list_v1_add_listener(toplevel_list, &list_listener, NULL);
	/* The first roundtrip delivers the existing toplevels (and asks for their
	 * addresses from the callback); the next ones collect the replies. */
	for (int i = 0; i < 3; i++) {
		if (wl_display_roundtrip(display) < 0) {
			fprintf(stderr, "hyprnav-capture: display error\n");
			return 2;
		}
	}

	if (run_mode == MODE_RESOLVE) {
		for (struct toplevel *entry = toplevels; entry; entry = entry->next) {
			if (entry->announced && entry->address == wanted_address) {
				printf("%s\n", entry->identifier);
				return 0;
			}
		}
		return 3;
	}

	for (struct toplevel *entry = toplevels; entry; entry = entry->next) {
		if (entry->announced) {
			emit_add(entry);
		}
	}
	if (run_mode == MODE_LIST) {
		return 0;
	}
	fputs("{\"ev\":\"ready\"}\n", stdout);
	fflush(stdout);

	jpeg = tj3Init(TJINIT_COMPRESS);
	if (!jpeg) {
		fprintf(stderr, "hyprnav-capture: cannot initialise the JPEG encoder\n");
		return 2;
	}

	int flags = fcntl(STDIN_FILENO, F_GETFL, 0);
	if (flags >= 0) {
		fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK);
	}

	struct pollfd fds[2];
	fds[0].fd = wl_display_get_fd(display);
	fds[0].events = POLLIN;
	fds[1].fd = STDIN_FILENO;
	fds[1].events = POLLIN;
	int nfds = 2;

	while (!stop_requested) {
		while (wl_display_prepare_read(display) != 0) {
			if (wl_display_dispatch_pending(display) < 0) {
				return 0;
			}
		}
		if (wl_display_flush(display) < 0 && errno != EAGAIN) {
			wl_display_cancel_read(display);
			break;
		}
		int polled = poll(fds, (nfds_t)nfds, poll_timeout_ms());
		if (polled < 0) {
			wl_display_cancel_read(display);
			if (errno == EINTR) {
				continue;
			}
			break;
		}
		if (fds[0].revents & POLLIN) {
			if (wl_display_read_events(display) < 0) {
				break;
			}
		} else {
			wl_display_cancel_read(display);
		}
		if (wl_display_dispatch_pending(display) < 0) {
			break;
		}
		if (nfds == 2 && (fds[1].revents & (POLLIN | POLLHUP))) {
			if (!drain_stdin()) {
				/* The daemon closed the command channel: nothing more to do. */
				break;
			}
		}
		service_rate_limited_captures();
	}
	return 0;
}
