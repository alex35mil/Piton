// pi's half of a lift (#408): a Node-API module that consumes a sandbox extension token in pi's process and releases it.
// The daemon issues every token, bound to pi's process, and writes this module to `<state>/native/lift.node`, which
// the pi package loads with `process.dlopen` (`pi/extensions/__lib/lift.ts`).
//
//   consume(token: string): number   the handle, or throws with errno's message
//   release(handle: number): void    throws when the handle isn't one
//
// Built by `cli/build.rs` with `-undefined dynamic_lookup`: Node supplies the `napi_*` symbols when it loads the
// module, so the few used here are declared below instead of including Node's headers (stable Node-API, version 1).

#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct napi_env__ *napi_env;
typedef struct napi_value__ *napi_value;
typedef struct napi_callback_info__ *napi_callback_info;
typedef int napi_status;
typedef napi_value (*napi_callback)(napi_env env, napi_callback_info info);

#define NAPI_OK 0
#define NAPI_AUTO_LENGTH SIZE_MAX

extern napi_status napi_get_cb_info(napi_env, napi_callback_info, size_t *, napi_value *, napi_value *, void **);
extern napi_status napi_get_value_string_utf8(napi_env, napi_value, char *, size_t, size_t *);
extern napi_status napi_get_value_int64(napi_env, napi_value, int64_t *);
extern napi_status napi_create_int64(napi_env, int64_t, napi_value *);
extern napi_status napi_get_undefined(napi_env, napi_value *);
extern napi_status napi_throw_error(napi_env, const char *, const char *);
extern napi_status napi_create_function(napi_env, const char *, size_t, napi_callback, void *, napi_value *);
extern napi_status napi_set_named_property(napi_env, napi_value, const char *, napi_value);

extern int64_t sandbox_extension_consume(const char *token);
extern int sandbox_extension_release(int64_t handle);

static napi_value fail(napi_env env, const char *what, int error) {
    char message[256];
    snprintf(message, sizeof message, "%s: %s", what, strerror(error));
    napi_throw_error(env, NULL, message);
    return NULL;
}

static napi_value consume(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value argv[1];
    if (napi_get_cb_info(env, info, &argc, argv, NULL, NULL) != NAPI_OK || argc < 1) {
        napi_throw_error(env, NULL, "consume(token) needs a token");
        return NULL;
    }
    size_t length = 0;
    if (napi_get_value_string_utf8(env, argv[0], NULL, 0, &length) != NAPI_OK) {
        napi_throw_error(env, NULL, "consume(token): the token must be a string");
        return NULL;
    }
    char *token = malloc(length + 1);
    if (!token) return fail(env, "consume", ENOMEM);
    napi_get_value_string_utf8(env, argv[0], token, length + 1, &length);
    errno = 0;
    int64_t handle = sandbox_extension_consume(token);
    int error = errno;
    // The token is pi's alone (bound to its process), but it's still not kept.
    memset(token, 0, length);
    free(token);
    if (handle < 0) return fail(env, "sandbox_extension_consume", error ? error : EPERM);
    napi_value result;
    napi_create_int64(env, handle, &result);
    return result;
}

static napi_value release(napi_env env, napi_callback_info info) {
    size_t argc = 1;
    napi_value argv[1];
    int64_t handle;
    if (napi_get_cb_info(env, info, &argc, argv, NULL, NULL) != NAPI_OK || argc < 1 ||
        napi_get_value_int64(env, argv[0], &handle) != NAPI_OK) {
        napi_throw_error(env, NULL, "release(handle) needs a handle");
        return NULL;
    }
    errno = 0;
    if (sandbox_extension_release(handle) != 0) return fail(env, "sandbox_extension_release", errno ? errno : EINVAL);
    napi_value undefined;
    napi_get_undefined(env, &undefined);
    return undefined;
}

// Node finds a Node-API module by this exported symbol.
__attribute__((visibility("default"))) napi_value napi_register_module_v1(napi_env env, napi_value exports) {
    napi_value fn;
    napi_create_function(env, "consume", NAPI_AUTO_LENGTH, consume, NULL, &fn);
    napi_set_named_property(env, exports, "consume", fn);
    napi_create_function(env, "release", NAPI_AUTO_LENGTH, release, NULL, &fn);
    napi_set_named_property(env, exports, "release", fn);
    return exports;
}

// The Node-API version this module needs.
__attribute__((visibility("default"))) int32_t node_api_module_get_api_version_v1(void) { return 1; }
