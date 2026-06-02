#define _POSIX_C_SOURCE 200809L

#include "shy_file.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static i32 set_filename(ShyFile *file, const char *filename) {
    if (file == NULL || filename == NULL) {
        errno = EINVAL;
        return -1;
    }

    usize len = strlen(filename);
    if (len == 0 || len >= SHY_FILE_NAME_MAX) {
        errno = EINVAL;
        return -1;
    }

    memcpy(file->filename, filename, len + 1);
    return 0;
}

static i32 validate_filename(const char *filename) {
    if (filename == NULL) {
        errno = EINVAL;
        return -1;
    }

    usize len = strlen(filename);
    if (len == 0 || len >= SHY_FILE_NAME_MAX) {
        errno = EINVAL;
        return -1;
    }

    return 0;
}

static bool slice_overlaps_mapping(const u8 *data, i32 len, const u8 *mapping, i32 mapping_len) {
    if (data == NULL || len <= 0 || mapping == NULL || mapping_len <= 0) {
        return false;
    }

    usize data_start = (usize)data;
    usize data_end = data_start + (usize)len;
    usize mapping_start = (usize)mapping;
    usize mapping_end = mapping_start + (usize)mapping_len;

    return data_start < mapping_end && mapping_start < data_end;
}

ShyFile *shy_open(const char *filename) {
    ShyFile *file = calloc(1, sizeof(*file));
    if (file == NULL) {
        return NULL;
    }

    if (set_filename(file, filename) != 0) {
        free(file);
        return NULL;
    }

    int fd = open(filename, O_RDWR | O_CREAT, 0644);
    if (fd < 0) {
        free(file);
        return NULL;
    }

    struct stat st;
    if (fstat(fd, &st) != 0 || st.st_size < 0 || st.st_size > INT32_MAX) {
        close(fd);
        free(file);
        return NULL;
    }

    file->len = (i32)st.st_size;
    if (file->len > 0) {
        void *mapped = mmap(NULL, (usize)file->len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (mapped == MAP_FAILED) {
            close(fd);
            free(file);
            return NULL;
        }
        file->content = mapped;
    }

    close(fd);
    return file;
}

i32 shy_flush(ShyFile *file) {
    if (file == NULL) {
        errno = EINVAL;
        return -1;
    }
    if (file->content == NULL || file->len == 0) {
        return 0;
    }

    return msync(file->content, (usize)file->len, MS_SYNC) == 0 ? 0 : -1;
}

i32 shy_close(ShyFile *file) {
    if (file == NULL) {
        errno = EINVAL;
        return -1;
    }

    i32 result = shy_flush(file);
    if (file->content != NULL && file->len > 0) {
        if (munmap(file->content, (usize)file->len) != 0) {
            result = -1;
        }
    }

    free(file);
    return result;
}

i32 shy_rename(ShyFile *file, const char *new_name) {
    if (file == NULL || new_name == NULL) {
        errno = EINVAL;
        return -1;
    }
    if (validate_filename(new_name) != 0) {
        return -1;
    }

    char old_name[SHY_FILE_NAME_MAX];
    memcpy(old_name, file->filename, sizeof(old_name));

    if (rename(old_name, new_name) != 0) {
        return -1;
    }

    if (set_filename(file, new_name) != 0) {
        rename(new_name, old_name);
        return -1;
    }

    return 0;
}

i32 shy_push_back(ShyFile *file, u8 byte) {
    return shy_push_back_slice(file, &byte, 1);
}

i32 shy_push_back_slice(ShyFile *file, const u8 *data, i32 len) {
    if (file == NULL || data == NULL || len < 0 || len > INT32_MAX - file->len) {
        errno = EINVAL;
        return -1;
    }
    if (len == 0) {
        return 0;
    }

    i32 old_len = file->len;
    i32 new_len = old_len + len;
    const u8 *append_data = data;
    u8 *owned_append_data = NULL;

    if (slice_overlaps_mapping(data, len, file->content, old_len)) {
        owned_append_data = malloc((usize)len);
        if (owned_append_data == NULL) {
            return -1;
        }
        memcpy(owned_append_data, data, (usize)len);
        append_data = owned_append_data;
    }

    int fd = open(file->filename, O_RDWR | O_CREAT, 0644);
    if (fd < 0) {
        free(owned_append_data);
        return -1;
    }

    if (file->content != NULL && old_len > 0 && msync(file->content, (usize)old_len, MS_SYNC) != 0) {
        free(owned_append_data);
        close(fd);
        return -1;
    }

    if (ftruncate(fd, (off_t)new_len) != 0) {
        free(owned_append_data);
        close(fd);
        return -1;
    }

    void *mapped = mmap(NULL, (usize)new_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (mapped == MAP_FAILED) {
        ftruncate(fd, (off_t)old_len);
        free(owned_append_data);
        close(fd);
        return -1;
    }

    if (file->content != NULL && old_len > 0) {
        if (munmap(file->content, (usize)old_len) != 0) {
            munmap(mapped, (usize)new_len);
            ftruncate(fd, (off_t)old_len);
            free(owned_append_data);
            close(fd);
            return -1;
        }
    }

    close(fd);

    file->content = mapped;
    file->len = new_len;
    memcpy(file->content + old_len, append_data, (usize)len);
    free(owned_append_data);
    return 0;
}

const u8 *shy_get_raw(ShyFile *file) {
    return file == NULL ? NULL : file->content;
}
