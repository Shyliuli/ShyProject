#![no_main]

#include <stdint.shyh>
#include "fs.h"
#include "lock.h"
#include "memlayout.h"
#include "proc.h"

#include <string.shyh>

#define NFILE 16
#define NPIPE 4
#define PIPESIZE 128
#define NMEMFILE 16
#define MEM_NAME_MAX 32
#define MEM_FILE_MAX 1024
#define FILE_BLOCKED -2

static Lock file_lock;
static struct file file_table[NFILE];

struct Pipe {
    unsigned char data[PIPESIZE];
    unsigned int nread;
    unsigned int nwrite;
    int read_open;
    int write_open;
    int used;
};

static struct Pipe pipe_table[NPIPE];

struct mem_file {
    char name[MEM_NAME_MAX];
    unsigned char data[MEM_FILE_MAX];
    unsigned int size;
    int used;
    int is_dir;
};

static struct mem_file mem_files[NMEMFILE];

impl Pipe {
    int empty(self *p) {
        return p.nread == p.nwrite;
    }

    int full(self *p) {
        return p.nwrite == p.nread + PIPESIZE;
    }

    int read_chan(self *p) {
        return (int)&p.nread;
    }

    int write_chan(self *p) {
        return (int)&p.nwrite;
    }

    void init(self *p) {
        p.nread = 0;
        p.nwrite = 0;
        p.read_open = 1;
        p.write_open = 1;
        p.used = 1;
    }

    void free(self *p) {
        p.nread = 0;
        p.nwrite = 0;
        p.read_open = 0;
        p.write_open = 0;
        p.used = 0;
    }

    void dup_end(self *p, int writable) {
        if (writable) {
            p.write_open = p.write_open + 1;
        } else {
            p.read_open = p.read_open + 1;
        }
    }

    void close_end(self *p, int writable) {
        if (writable) {
            if (p.write_open > 0) {
                p.write_open = p.write_open - 1;
            }
            wakeup(p.read_chan());
        } else {
            if (p.read_open > 0) {
                p.read_open = p.read_open - 1;
            }
            wakeup(p.write_chan());
        }
    }

    int read(self *p, void *user_buf, unsigned int n) {
        unsigned int addr = (unsigned int)user_buf;
        unsigned char *dst;
        unsigned int i = 0;

        if (!user_range_ok(addr, n)) {
            return -1;
        }
        if (n == 0) {
            return 0;
        }

        while (p.empty()) {
            if (p.write_open == 0) {
                return 0;
            }
            lock_release(&file_lock);
            sleep(p.read_chan());
            return FILE_BLOCKED;
        }

        dst = (unsigned char *)(current->segbase + addr);
        while (i < n && !p.empty()) {
            dst[i] = p.data[p.nread % PIPESIZE];
            p.nread = p.nread + 1;
            i = i + 1;
        }
        wakeup(p.write_chan());
        return (int)i;
    }

    int write(self *p, const void *user_buf, unsigned int n) {
        unsigned int addr = (unsigned int)user_buf;
        unsigned char *src;
        unsigned int i = 0;

        if (!user_range_ok(addr, n)) {
            return -1;
        }
        if (n == 0) {
            return 0;
        }
        if (p.read_open == 0) {
            return -1;
        }

        src = (unsigned char *)(current->segbase + addr);
        while (i < n) {
            if (p.full()) {
                wakeup(p.read_chan());
                if (i > 0) {
                    return (int)i;
                }
                if (p.read_open == 0) {
                    return -1;
                }
                lock_release(&file_lock);
                sleep(p.write_chan());
                return FILE_BLOCKED;
            }
            p.data[p.nwrite % PIPESIZE] = src[i];
            p.nwrite = p.nwrite + 1;
            i = i + 1;
        }
        wakeup(p.read_chan());
        return (int)i;
    }
}

static int streq(const char *a, const char *b) {
    int i = 0;

    while (a[i] && b[i]) {
        if (a[i] != b[i]) {
            return 0;
        }
        i = i + 1;
    }
    return a[i] == b[i];
}

static int user_range_ok(unsigned int addr, unsigned int n) {
    if (addr > USER_SEG_SIZE) {
        return 0;
    }
    if (n > USER_SEG_SIZE) {
        return 0;
    }
    if (addr + n < addr) {
        return 0;
    }
    if (addr + n > USER_SEG_SIZE) {
        return 0;
    }
    return 1;
}

static char *user_string(const char *path) {
    unsigned int addr = (unsigned int)path;
    char *s;

    if (addr >= USER_SEG_SIZE) {
        return 0;
    }

    s = (char *)(current->segbase + addr);
    for (unsigned int i = addr; i < USER_SEG_SIZE; i++) {
        if (s[i - addr] == 0) {
            return s;
        }
    }
    return 0;
}

static char *user_path_name(const char *path) {
    char *path_name = user_string(path);

    if (path_name == 0) {
        return 0;
    }
    if (path_name[0] == '/') {
        path_name = path_name + 1;
    }
    if (path_name[0] == 0) {
        return 0;
    }
    return path_name;
}

static int valid_mem_name(const char *name) {
    int i = 0;

    while (name[i]) {
        if (i >= MEM_NAME_MAX - 1) {
            return 0;
        }
        if (name[i] == '/') {
            return 0;
        }
        i = i + 1;
    }
    return i > 0;
}

static void copy_mem_name(char *dst, const char *src) {
    int i = 0;

    while (i < MEM_NAME_MAX - 1 && src[i]) {
        dst[i] = src[i];
        i = i + 1;
    }
    dst[i] = 0;
}

static struct mem_file *mem_lookup_locked(const char *name) {
    for (int i = 0; i < NMEMFILE; i++) {
        if (mem_files[i].used && streq(mem_files[i].name, name)) {
            return &mem_files[i];
        }
    }
    return 0;
}

static struct mem_file *mem_alloc_locked(void) {
    for (int i = 0; i < NMEMFILE; i++) {
        if (!mem_files[i].used) {
            return &mem_files[i];
        }
    }
    return 0;
}

struct fs_file *fs_lookup(const char *path_name) {
    for (int i = 0; i < fs_nfiles; i++) {
        if (streq(fs_files[i].name, path_name)) {
            return &fs_files[i];
        }
    }
    return 0;
}

static int copy_to_user_string(char *user_buf, int max, const char *src) {
    unsigned int addr = (unsigned int)user_buf;
    char *dst;
    int i = 0;

    if (max <= 0) {
        return -1;
    }
    if (!user_range_ok(addr, (unsigned int)max)) {
        return -1;
    }
    dst = (char *)(current->segbase + addr);
    while (i < max - 1 && src[i]) {
        dst[i] = src[i];
        i = i + 1;
    }
    dst[i] = 0;
    return 0;
}

int fs_dirent(int index, char *user_buf, int max) {
    int seen = 0;

    if (index < 0) {
        return 0;
    }
    if (index < fs_nfiles) {
        if (copy_to_user_string(user_buf, max, fs_files[index].name) < 0) {
            return -1;
        }
        return 1;
    }

    seen = fs_nfiles;
    lock_acquire(&file_lock);
    for (int i = 0; i < NMEMFILE; i++) {
        if (mem_files[i].used) {
            if (seen == index) {
                int r = copy_to_user_string(user_buf, max, mem_files[i].name);
                lock_release(&file_lock);
                if (r < 0) {
                    return -1;
                }
                return 1;
            }
            seen = seen + 1;
        }
    }
    lock_release(&file_lock);
    return 0;
}

int fs_mkdir(const char *path) {
    char *name = user_path_name(path);
    struct mem_file *mf;

    if (name == 0 || !valid_mem_name(name)) {
        return -1;
    }
    if (fs_lookup(name) != 0) {
        return -1;
    }

    lock_acquire(&file_lock);
    if (mem_lookup_locked(name) != 0) {
        lock_release(&file_lock);
        return -1;
    }
    mf = mem_alloc_locked();
    if (mf == 0) {
        lock_release(&file_lock);
        return -1;
    }
    copy_mem_name(mf->name, name);
    mf->size = 0;
    mf->used = 1;
    mf->is_dir = 1;
    lock_release(&file_lock);
    return 0;
}

int fs_rm(const char *path) {
    char *name = user_path_name(path);
    struct mem_file *mf;

    if (name == 0 || !valid_mem_name(name)) {
        return -1;
    }

    lock_acquire(&file_lock);
    mf = mem_lookup_locked(name);
    if (mf == 0) {
        lock_release(&file_lock);
        return -1;
    }
    mf->used = 0;
    mf->size = 0;
    mf->is_dir = 0;
    mf->name[0] = 0;
    lock_release(&file_lock);
    return 0;
}

static int resolve_fd(int fd) {
    if (fd == 0) {
        return current->stdin_fd;
    }
    if (fd == 1) {
        return current->stdout_fd;
    }
    return fd;
}

static unsigned int fd_bit(int fd) {
    return (unsigned int)1 << fd;
}

static int current_owns_fd(int fd) {
    if (fd < 3 || fd >= NFILE) {
        return 0;
    }
    return (current->open_fds & fd_bit(fd)) != 0;
}

static int fd_available_to_current(int original, int actual) {
    if (original == 0 && current->stdin_fd == actual) {
        return 1;
    }
    if (original == 1 && current->stdout_fd == actual) {
        return 1;
    }
    return current_owns_fd(actual);
}

static void current_add_fd(int fd) {
    if (fd >= 3 && fd < NFILE) {
        current->open_fds = current->open_fds | fd_bit(fd);
    }
}

static void current_del_fd(int fd) {
    if (fd >= 3 && fd < NFILE) {
        current->open_fds = current->open_fds & ~fd_bit(fd);
    }
}

int file_result_blocked(int r) {
    return r == FILE_BLOCKED;
}

int file_open(const char *path) {
    char *path_name = user_path_name(path);
    struct fs_file *fsf;
    struct mem_file *mf;
    int fd;

    if (path_name == 0) {
        return -1;
    }

    lock_acquire(&file_lock);
    mf = mem_lookup_locked(path_name);
    if (mf != 0 && mf->is_dir) {
        lock_release(&file_lock);
        return -1;
    }
    for (fd = 3; fd < NFILE; fd++) {
        if (file_table[fd].ref == 0) {
            if (mf != 0) {
                file_table[fd].type = FD_MEM;
                file_table[fd].file = 0;
                file_table[fd].mem = mf;
            } else {
                fsf = fs_lookup(path_name);
                if (fsf == 0) {
                    lock_release(&file_lock);
                    return -1;
                }
                file_table[fd].type = FD_REG;
                file_table[fd].file = fsf;
                file_table[fd].mem = 0;
            }
            file_table[fd].pipe = 0;
            file_table[fd].off = 0;
            file_table[fd].ref = 1;
            file_table[fd].readable = 1;
            file_table[fd].writable = 0;
            current_add_fd(fd);
            lock_release(&file_lock);
            return fd;
        }
    }
    lock_release(&file_lock);

    return -1;
}

int file_create(const char *path) {
    char *path_name = user_path_name(path);
    struct mem_file *mf;
    int fd;

    if (path_name == 0 || !valid_mem_name(path_name)) {
        return -1;
    }

    lock_acquire(&file_lock);
    mf = mem_lookup_locked(path_name);
    if (mf != 0 && mf->is_dir) {
        lock_release(&file_lock);
        return -1;
    }
    if (mf == 0) {
        mf = mem_alloc_locked();
        if (mf == 0) {
            lock_release(&file_lock);
            return -1;
        }
        copy_mem_name(mf->name, path_name);
        mf->used = 1;
        mf->is_dir = 0;
    }
    mf->size = 0;

    for (fd = 3; fd < NFILE; fd++) {
        if (file_table[fd].ref == 0) {
            file_table[fd].type = FD_MEM;
            file_table[fd].file = 0;
            file_table[fd].pipe = 0;
            file_table[fd].mem = mf;
            file_table[fd].off = 0;
            file_table[fd].ref = 1;
            file_table[fd].readable = 1;
            file_table[fd].writable = 1;
            current_add_fd(fd);
            lock_release(&file_lock);
            return fd;
        }
    }
    lock_release(&file_lock);
    return -1;
}

int file_read(int fd, void *user_buf, unsigned int n) {
    struct file *f;
    unsigned int addr = (unsigned int)user_buf;
    unsigned int remaining;
    unsigned char *dst;
    int r;

    int original_fd = fd;
    fd = resolve_fd(fd);
    if (fd < 3 || fd >= NFILE || !fd_available_to_current(original_fd, fd)) {
        return -1;
    }
    lock_acquire(&file_lock);
    f = &file_table[fd];
    if (f->ref <= 0 || !f->readable) {
        lock_release(&file_lock);
        return -1;
    }
    if (f->type == FD_PIPE) {
        r = f->pipe.read(user_buf, n);
        if (r != FILE_BLOCKED) {
            lock_release(&file_lock);
        }
        return r;
    }
    if (f->type != FD_REG || f->file == 0) {
        if (f->type != FD_MEM || f->mem == 0) {
            lock_release(&file_lock);
            return -1;
        }
        if (!user_range_ok(addr, n)) {
            lock_release(&file_lock);
            return -1;
        }
        if (f->off >= f->mem->size) {
            lock_release(&file_lock);
            return 0;
        }
        remaining = f->mem->size - f->off;
        if (n > remaining) {
            n = remaining;
        }
        dst = (unsigned char *)(current->segbase + addr);
        memcpy(dst, f->mem->data + f->off, n);
        f->off = f->off + n;
        lock_release(&file_lock);
        return (int)n;
    }

    if (!user_range_ok(addr, n)) {
        lock_release(&file_lock);
        return -1;
    }
    if (f->off >= f->file->size) {
        lock_release(&file_lock);
        return 0;
    }

    remaining = f->file->size - f->off;
    if (n > remaining) {
        n = remaining;
    }
    dst = (unsigned char *)(current->segbase + addr);
    memcpy(dst, f->file->data + f->off, n);
    f->off = f->off + n;
    lock_release(&file_lock);
    return (int)n;
}

int file_write(int fd, const void *user_buf, unsigned int n) {
    struct file *f;
    int r;
    unsigned int addr = (unsigned int)user_buf;
    unsigned int space;
    unsigned char *src;

    int original_fd = fd;
    fd = resolve_fd(fd);
    if (fd < 3 || fd >= NFILE || !fd_available_to_current(original_fd, fd)) {
        return -1;
    }
    lock_acquire(&file_lock);
    f = &file_table[fd];
    if (f->ref <= 0 || !f->writable) {
        lock_release(&file_lock);
        return -1;
    }
    if (f->type == FD_PIPE) {
        r = f->pipe.write(user_buf, n);
        if (r != FILE_BLOCKED) {
            lock_release(&file_lock);
        }
        return r;
    }
    if (f->type == FD_MEM && f->mem != 0) {
        if (!user_range_ok(addr, n)) {
            lock_release(&file_lock);
            return -1;
        }
        if (f->off >= MEM_FILE_MAX) {
            lock_release(&file_lock);
            return 0;
        }
        space = MEM_FILE_MAX - f->off;
        if (n > space) {
            n = space;
        }
        src = (unsigned char *)(current->segbase + addr);
        memcpy(f->mem->data + f->off, src, n);
        f->off = f->off + n;
        if (f->off > f->mem->size) {
            f->mem->size = f->off;
        }
        lock_release(&file_lock);
        return (int)n;
    }
    lock_release(&file_lock);
    return -1;
}

int file_close(int fd) {
    struct file *f;

    if (fd < 0 || fd >= NFILE) {
        return -1;
    }
    lock_acquire(&file_lock);
    f = &file_table[fd];
    if (f->ref <= 0) {
        lock_release(&file_lock);
        return -1;
    }

    f->ref = f->ref - 1;
    if (f->type == FD_PIPE && f->pipe != 0) {
        f->pipe.close_end(f->writable);
    }
    if (f->ref == 0) {
        if (f->type == FD_PIPE && f->pipe != 0 &&
            f->pipe.read_open == 0 && f->pipe.write_open == 0) {
            f->pipe.free();
        }
        f->type = FD_NONE;
        f->file = 0;
        f->pipe = 0;
        f->mem = 0;
        f->off = 0;
        f->readable = 0;
        f->writable = 0;
    }
    lock_release(&file_lock);
    return 0;
}

int file_close_fd(int fd) {
    int actual;
    int r;

    if (fd == 0) {
        actual = current->stdin_fd;
        current->stdin_fd = -1;
        if (actual >= 3) {
            return file_close(actual);
        }
        return 0;
    }
    if (fd == 1) {
        actual = current->stdout_fd;
        current->stdout_fd = -1;
        if (actual >= 3) {
            return file_close(actual);
        }
        return 0;
    }

    if (!current_owns_fd(fd)) {
        return -1;
    }
    current_del_fd(fd);
    r = file_close(fd);
    return r;
}

void file_close_proc(void) {
    unsigned int mask = current->open_fds;

    file_close_fd(0);
    file_close_fd(1);

    for (int fd = 3; fd < NFILE; fd++) {
        if ((mask & fd_bit(fd)) != 0) {
            current_del_fd(fd);
            file_close(fd);
        }
    }
}

int file_dup(int fd) {
    struct file *f;

    if (fd < 3 || fd >= NFILE) {
        return -1;
    }
    lock_acquire(&file_lock);
    f = &file_table[fd];
    if (f->ref <= 0) {
        lock_release(&file_lock);
        return -1;
    }
    f->ref = f->ref + 1;
    if (f->type == FD_PIPE && f->pipe != 0) {
        f->pipe.dup_end(f->writable);
    }
    lock_release(&file_lock);
    return fd;
}

void file_dup_open_mask(unsigned int mask) {
    for (int fd = 3; fd < NFILE; fd++) {
        if ((mask & fd_bit(fd)) != 0) {
            file_dup(fd);
        }
    }
}

int file_dup2(int oldfd, int newfd) {
    int actual = resolve_fd(oldfd);
    int r;

    if (newfd != 0 && newfd != 1) {
        return -1;
    }
    if (actual < 3 || actual >= NFILE || !fd_available_to_current(oldfd, actual)) {
        return -1;
    }

    r = file_dup(actual);
    if (r < 0) {
        return -1;
    }

    file_close_fd(newfd);
    if (newfd == 0) {
        current->stdin_fd = actual;
    } else {
        current->stdout_fd = actual;
    }
    return newfd;
}

int file_pipe(int *fds) {
    unsigned int addr = (unsigned int)fds;
    int rfd = -1;
    int wfd = -1;
    struct Pipe *p = 0;
    int *ufds;

    if (!user_range_ok(addr, 8)) {
        return -1;
    }

    lock_acquire(&file_lock);
    for (int i = 0; i < NPIPE; i++) {
        if (!pipe_table[i].used) {
            p = &pipe_table[i];
            break;
        }
    }
    if (p == 0) {
        lock_release(&file_lock);
        return -1;
    }

    for (int fd = 3; fd + 1 < NFILE; fd++) {
        if (file_table[fd].ref == 0 && file_table[fd + 1].ref == 0) {
            rfd = fd;
            wfd = fd + 1;
            break;
        }
    }
    if (rfd < 0) {
        lock_release(&file_lock);
        return -1;
    }

    p.init();

    file_table[rfd].type = FD_PIPE;
    file_table[rfd].file = 0;
    file_table[rfd].pipe = p;
    file_table[rfd].mem = 0;
    file_table[rfd].off = 0;
    file_table[rfd].ref = 1;
    file_table[rfd].readable = 1;
    file_table[rfd].writable = 0;

    file_table[wfd].type = FD_PIPE;
    file_table[wfd].file = 0;
    file_table[wfd].pipe = p;
    file_table[wfd].mem = 0;
    file_table[wfd].off = 0;
    file_table[wfd].ref = 1;
    file_table[wfd].readable = 0;
    file_table[wfd].writable = 1;

    ufds = (int *)(current->segbase + addr);
    ufds[0] = rfd;
    ufds[1] = wfd;
    current_add_fd(rfd);
    current_add_fd(wfd);
    lock_release(&file_lock);
    return 0;
}

static int copy_user_arg(const char *arg, unsigned char *buf) {
    unsigned int addr;
    unsigned char *src;

    buf[0] = 0;
    if (arg == 0) {
        return 0;
    }

    addr = (unsigned int)arg;
    if (addr >= USER_SEG_SIZE) {
        return -1;
    }

    src = (unsigned char *)(current->segbase + addr);
    for (unsigned int i = 0; i < USER_ARG_MAX - 1; i++) {
        if (addr + i >= USER_SEG_SIZE) {
            return -1;
        }
        buf[i] = src[i];
        if (buf[i] == 0) {
            return 0;
        }
    }
    buf[USER_ARG_MAX - 1] = 0;
    return 0;
}

static int copy_exec_name(const char *path, unsigned char *buf) {
    char *name = user_path_name(path);
    int i = 0;

    buf[0] = 0;
    if (name == 0) {
        return -1;
    }
    while (i < USER_ARG_MAX - 1 && name[i]) {
        buf[i] = (unsigned char)name[i];
        i = i + 1;
    }
    buf[i] = 0;
    return 0;
}

static unsigned int read_be32(unsigned char *p, unsigned int off) {
    unsigned int v = 0;

    v = v | ((unsigned int)p[off + 0] << 24);
    v = v | ((unsigned int)p[off + 1] << 16);
    v = v | ((unsigned int)p[off + 2] << 8);
    v = v | (unsigned int)p[off + 3];
    return v;
}

unsigned int user_image_stack_size(struct fs_file *fsf) {
    unsigned int stack_size = USER_STACK_DEFAULT_SIZE;

    if (fsf != 0 && fsf->size >= 12) {
        stack_size = read_be32(fsf->data, 8);
        if (stack_size == 0) {
            stack_size = USER_STACK_DEFAULT_SIZE;
        }
    }

    if (stack_size > USER_STACK_MAX_SIZE) {
        return 0;
    }
    if (stack_size > USER_STACK_LIMIT) {
        return 0;
    }
    return stack_size;
}

unsigned int user_image_stack_base(struct fs_file *fsf) {
    unsigned int stack_size = user_image_stack_size(fsf);

    if (stack_size == 0) {
        return 0;
    }
    return USER_STACK_BASE(stack_size);
}

int user_image_layout_ok(struct fs_file *fsf) {
    unsigned int mem_hint = USER_SEG_SIZE;

    if (fsf == 0) {
        return 0;
    }
    if (fsf->size >= USER_SEG_SIZE) {
        return 0;
    }
    if (fsf->size > USER_ARG_ADDR) {
        return 0;
    }
    if (fsf->size >= 8) {
        mem_hint = read_be32(fsf->data, 4);
        if (mem_hint == 0) {
            mem_hint = USER_SEG_SIZE;
        }
    }
    if (mem_hint > USER_SEG_SIZE) {
        return 0;
    }
    if (user_image_stack_base(fsf) == 0) {
        return 0;
    }
    return 1;
}

int sys_exec(const char *path, const char *arg) {
    unsigned char namebuf[USER_ARG_MAX];
    unsigned char argbuf[USER_ARG_MAX];
    int fd;
    struct fs_file *fsf;
    unsigned char *dst;
    unsigned int stack_base;

    if (copy_exec_name(path, namebuf) < 0) {
        return -1;
    }
    if (copy_user_arg(arg, argbuf) < 0) {
        return -1;
    }
    fd = file_open(path);
    if (fd < 0) {
        return -1;
    }

    lock_acquire(&file_lock);
    fsf = file_table[fd].file;
    lock_release(&file_lock);
    if (!user_image_layout_ok(fsf)) {
        file_close_fd(fd);
        return -1;
    }
    stack_base = user_image_stack_base(fsf);

    dst = (unsigned char *)current->segbase;
    memcpy(dst, fsf->data, fsf->size);
    memcpy(dst + USER_ARG0_ADDR, namebuf, USER_ARG_MAX);
    memcpy(dst + USER_ARG_ADDR, argbuf, USER_ARG_MAX);
    asm("fencei");
    file_close_fd(fd);

    proc_set_current_name((const char *)namebuf);
    proc_set_current_stack_base(stack_base);
    ctx_reset_current(stack_base);
    proc_set_current_size(fsf->size);

    return 0;
}
