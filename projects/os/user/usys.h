#ifndef SHYOS_USER_USYS_H
#define SHYOS_USER_USYS_H

#define SYS_fork 1
#define SYS_yield 2
#define SYS_getpid 3
#define SYS_exit 4
#define SYS_write 5
#define SYS_read 6
#define SYS_exec 7
#define SYS_wait 8
#define SYS_open 9
#define SYS_close 10
#define SYS_kill 11
#define SYS_sleep 12
#define SYS_wakeup 13
#define SYS_p3counter 14
#define SYS_pipe 15
#define SYS_dup2 16
#define SYS_create 17
#define SYS_dirent 18
#define SYS_mkdir 19
#define SYS_rm 20
#define SYS_psline 21
#define SYS_sbrk 22

int write(int fd, const char *buf, int n);
int read(int fd, char *buf, int n);
int open(const char *path);
int close(int fd);
int fork(void);
int getpid(void);
void user_exit(int status);
#define exit user_exit
void yield(void);
int exec(const char *path, const char *arg);
int wait(int *status);
int kill(int pid);
int sleep(int chan);
int wakeup(int chan);
int p3counter(int op);
int pipe(int fds[2]);
int dup2(int oldfd, int newfd);
int create(const char *path);
int dirent(int index, char *buf, int max);
int mkdir(const char *path);
int rm(const char *path);
int psline(int index, char *buf, int max);
void *sbrk(int n);

#endif
