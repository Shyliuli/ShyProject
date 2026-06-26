#ifndef SHY_FILE_H
#define SHY_FILE_H

#include "shy_types.h"

#if defined(__cplusplus)
extern "C" {
#endif

#define SHY_FILE_NAME_MAX 256

typedef struct ShyFile {
    i32 len;
    char filename[SHY_FILE_NAME_MAX];
    u8 *content;
} ShyFile;

ShyFile *shy_open(const char *filename);
i32 shy_close(ShyFile *file);
i32 shy_flush(ShyFile *file);
i32 shy_rename(ShyFile *file, const char *new_name);
i32 shy_push_back(ShyFile *file, u8 byte);
i32 shy_push_back_slice(ShyFile *file, const u8 *data, i32 len);
i32 shy_len(ShyFile *file);

#if defined(__cplusplus)
}
#endif

#endif
