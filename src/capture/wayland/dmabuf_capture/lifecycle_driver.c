/* Test-only VA/GBM ownership ledger. Loaded only into isolated test children. */
#include <gbm.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int bos, surfaces, contexts, configs, displays, bo_sequence;
struct test_bo { int number; };

static void record(const char *format, ...)
{
    FILE *file = fopen(getenv("HYPR_RDP_DMABUF_LOG"), "a");
    if (!file) abort();
    va_list args;
    va_start(args, format);
    vfprintf(file, format, args);
    va_end(args);
    fputc('\n', file);
    fclose(file);
}
static int fails(const char *stage)
{
    const char *wanted = getenv("HYPR_RDP_DMABUF_FAIL");
    return wanted && !strcmp(wanted, stage);
}
struct gbm_device *gbm_create_device(int fd)
{
    (void)fd;
    if (fails("device")) return NULL;
    record("device+");
    return (struct gbm_device *)malloc(1);
}
void gbm_device_destroy(struct gbm_device *device)
{
    record("device- bos=%d displays=%d", bos, displays);
    free(device);
}
struct gbm_bo *gbm_bo_create(struct gbm_device *d, uint32_t w, uint32_t h,
                             uint32_t format, uint32_t flags)
{
    (void)d; (void)w; (void)h; (void)format; (void)flags;
    int number = ++bo_sequence;
    if ((number == 1 && fails("bo-1")) || (number == 2 && fails("bo-2"))) return NULL;
    struct test_bo *bo = malloc(sizeof(*bo));
    bo->number = number;
    bos++;
    record("bo+");
    return (struct gbm_bo *)bo;
}
void gbm_bo_destroy(struct gbm_bo *bo) { bos--; record("bo-"); free(bo); }
int gbm_bo_get_plane_count(struct gbm_bo *bo) { (void)bo; return 1; }
uint32_t gbm_bo_get_stride(struct gbm_bo *bo) { (void)bo; return 64; }
uint32_t gbm_bo_get_offset(struct gbm_bo *bo, int plane) { (void)bo; (void)plane; return 0; }
uint64_t gbm_bo_get_modifier(struct gbm_bo *bo) { (void)bo; return 0; }
int gbm_bo_get_fd(struct gbm_bo *bo)
{
    int n = ((struct test_bo *)bo)->number;
    if ((n == 1 && fails("fd-1")) || (n == 2 && fails("fd-2"))) return -1;
    return open("/dev/null", O_RDONLY | O_CLOEXEC);
}
VADisplay vaGetDisplayDRM(int fd) { (void)fd; if (fails("display")) return NULL; displays++; record("display+"); return (void *)1; }
VAStatus vaInitialize(VADisplay d, int *major, int *minor)
{
    (void)d; *major = 1; *minor = 0;
    return fails("initialize") ? VA_STATUS_ERROR_OPERATION_FAILED : VA_STATUS_SUCCESS;
}
VAStatus vaCreateConfig(VADisplay d, VAProfile p, VAEntrypoint e,
                        VAConfigAttrib *a, int n, VAConfigID *out)
{
    (void)d; (void)p; (void)e; (void)a; (void)n;
    if (fails("config")) return VA_STATUS_ERROR_OPERATION_FAILED;
    *out = 1; configs++; record("config+"); return VA_STATUS_SUCCESS;
}
VAStatus vaCreateSurfaces(VADisplay d, unsigned int fmt, unsigned int w, unsigned int h,
                          VASurfaceID *out, unsigned int n, VASurfaceAttrib *a, unsigned int na)
{
    (void)d; (void)fmt; (void)w; (void)h; (void)a; (void)na;
    const char *stage = surfaces == 0 ? "surface" : surfaces == 1 ? "import-1" : "import-2";
    if (fails(stage)) return VA_STATUS_ERROR_OPERATION_FAILED;
    for (unsigned int i = 0; i < n; i++) out[i] = 100 + surfaces++;
    record("surface+"); return VA_STATUS_SUCCESS;
}
VAStatus vaCreateContext(VADisplay d, VAConfigID c, int w, int h, int flags,
                         VASurfaceID *s, int n, VAContextID *out)
{
    (void)d; (void)c; (void)w; (void)h; (void)flags; (void)s; (void)n;
    if (fails("context")) return VA_STATUS_ERROR_OPERATION_FAILED;
    *out = 9; contexts++; record("context+"); return VA_STATUS_SUCCESS;
}
VAStatus vaDestroySurfaces(VADisplay d, VASurfaceID *s, int n)
{
    (void)d; (void)s;
    if (contexts) { record("ORDER_ERROR surface before context"); return VA_STATUS_ERROR_SURFACE_BUSY; }
    surfaces -= n; record("surface-"); return VA_STATUS_SUCCESS;
}
VAStatus vaDestroyContext(VADisplay d, VAContextID c)
{ (void)d; (void)c; contexts--; record("context-"); return VA_STATUS_SUCCESS; }
VAStatus vaDestroyConfig(VADisplay d, VAConfigID c)
{ (void)d; (void)c; configs--; record("config-"); return VA_STATUS_SUCCESS; }
VAStatus vaTerminate(VADisplay d)
{
    (void)d;
    /* Termination also owns any resources from incomplete construction. */
    surfaces = contexts = configs = 0; displays--; record("display-"); return VA_STATUS_SUCCESS;
}
VAStatus vaExportSurfaceHandle(VADisplay d, VASurfaceID s, uint32_t m, uint32_t f, void *out)
{
    (void)d; (void)s; (void)m; (void)f;
    if (fails("export")) return VA_STATUS_ERROR_OPERATION_FAILED;
    VADRMPRIMESurfaceDescriptor *desc = out;
    desc->fourcc = VA_FOURCC_NV12; desc->width = 16; desc->height = 16;
    desc->num_objects = fails("empty-objects") ? 0 : fails("multi-object") ? 2 : 1;
    desc->num_layers = fails("empty-layers") ? 0 : 1;
    for (unsigned int i = 0; i < desc->num_objects; i++) {
        desc->objects[i].fd = fails("invalid-fd") ? -1 : open("/dev/null", O_RDONLY | O_CLOEXEC);
        desc->objects[i].size = 384;
    }
    desc->layers[0].drm_format = 0x3231564e;
    desc->layers[0].num_planes = 2;
    desc->layers[0].object_index[1] = desc->num_objects - 1;
    desc->layers[0].offset[1] = desc->num_objects == 1 ? 256 : 0;
    desc->layers[0].pitch[0] = desc->layers[0].pitch[1] = 16;
    record("export objects=%u", desc->num_objects);
    return VA_STATUS_SUCCESS;
}
