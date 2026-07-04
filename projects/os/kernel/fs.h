#ifndef SHYOS_KERNEL_FS_H
#define SHYOS_KERNEL_FS_H

struct fs_file {
    char *name;
    unsigned char *data;
    unsigned int size;
};

struct Pipe;
struct mem_file;

enum file_type { FD_NONE, FD_REG, FD_PIPE, FD_MEM };

struct file {
    enum file_type type;
    struct fs_file *file;
    struct Pipe *pipe;
    struct mem_file *mem;
    unsigned int off;
    int ref;
    int readable;
    int writable;
};

extern struct fs_file fs_files[];
extern int fs_nfiles;

struct fs_file *fs_lookup(const char *path_name);
int file_open(const char *path);
int file_create(const char *path);
int file_read(int fd, void *user_buf, unsigned int n);
int file_write(int fd, const void *user_buf, unsigned int n);
int file_close(int fd);
int file_close_fd(int fd);
void file_close_proc(void);
int file_dup(int fd);
void file_dup_open_mask(unsigned int mask);
int file_dup2(int oldfd, int newfd);
int file_pipe(int *fds);
int file_result_blocked(int r);
int fs_dirent(int index, char *user_buf, int max);
int fs_mkdir(const char *path);
int fs_rm(const char *path);
unsigned int user_image_stack_size(struct fs_file *fsf);
unsigned int user_image_stack_base(struct fs_file *fsf);
int user_image_layout_ok(struct fs_file *fsf);
int sys_exec(const char *path, const char *arg);

#endif
