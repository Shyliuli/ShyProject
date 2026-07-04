#![no_main]
#![mem(1M)]
#![stack(16K)]

#include "usys.h"
#include "ulib.h"

#define MAXARGS 10
#define LINE_MAX 128

struct Args {
    char *argv[MAXARGS];
    int argc;
};

struct Text {
    const char *data;
};

impl Args {
    void clear(self *a) {
        a.argc = 0;
    }

    void add(self *a, char *s) {
        if (a.argc < MAXARGS) {
            a.argv[a.argc] = s;
            a.argc = a.argc + 1;
        }
    }

    char *get(self *a, int i) {
        if (i < 0 || i >= a.argc) {
            return (char *)0;
        }
        return a.argv[i];
    }

    int empty(self *a) {
        return a.argc <= 0;
    }

    char *shift(self *a) {
        char *first = a.get(0);

        if (first == 0) {
            return (char *)0;
        }
        for (int i = 1; i < a.argc; i++) {
            a.argv[i - 1] = a.argv[i];
        }
        a.argc = a.argc - 1;
        return first;
    }

    int parse(self *a, char *s) {
        int i = 0;

        a.clear();
        while (s[i]) {
            while (s[i] == ' ' || s[i] == '\t') {
                i = i + 1;
            }
            if (!s[i]) {
                break;
            }
            a.add(s + i);
            while (s[i] && s[i] != ' ' && s[i] != '\t') {
                i = i + 1;
            }
            if (s[i]) {
                s[i] = 0;
                i = i + 1;
            }
        }
        return a.argc;
    }
};

impl Text {
    int len(self *t) {
        int n = 0;

        while (t.data[n]) {
            n = n + 1;
        }
        return n;
    }

    int contains(self *t, const char *needle) {
        int i = 0;
        struct Text pat;
        int n;

        pat.data = needle;
        n = pat.len();
        if (n == 0) {
            return 1;
        }
        while (t.data[i]) {
            int j = 0;

            while (needle[j] && t.data[i + j] == needle[j]) {
                j = j + 1;
            }
            if (j == n) {
                return 1;
            }
            i = i + 1;
        }
        return 0;
    }
};

static char *arg_at(struct Args *a, int i) {
    if (i < 0 || i >= a->argc) {
        return (char *)0;
    }
    return a->argv[i];
}

static char *skip_spaces(char *s) {
    while (*s == ' ' || *s == '\t') {
        s = s + 1;
    }
    return s;
}

static void trim_right(char *s) {
    int n = 0;

    while (s[n]) {
        n = n + 1;
    }
    while (n > 0 && (s[n - 1] == ' ' || s[n - 1] == '\t')) {
        n = n - 1;
        s[n] = 0;
    }
}

static int strlen2(const char *s) {
    struct Text text;

    text.data = s;
    return text.len();
}

static void print_int(int value) {
    char buf[12];
    int i = 0;
    int n = value;

    if (n == 0) {
        puts("0");
        return;
    }
    if (n < 0) {
        puts("-");
        n = 0 - n;
    }
    while (n > 0 && i < 11) {
        buf[i] = (char)('0' + (n % 10));
        n = n / 10;
        i = i + 1;
    }
    while (i > 0) {
        i = i - 1;
        write(1, &buf[i], 1);
    }
}

static int parse_int(const char *s) {
    int n = 0;
    int i = 0;
    int neg = 0;

    if (s[0] == '-') {
        neg = 1;
        i = 1;
    }
    while (s[i] >= '0' && s[i] <= '9') {
        n = n * 10 + (int)(s[i] - '0');
        i = i + 1;
    }
    if (neg) {
        return 0 - n;
    }
    return n;
}

static int contains(const char *s, const char *needle) {
    struct Text text;

    text.data = s;
    return text.contains(needle);
}

static void make_path(const char *cmd, char *path) {
    int i = 0;

    path[0] = '/';
    while (cmd[i] && i < 30) {
        path[i + 1] = cmd[i];
        i = i + 1;
    }
    path[i + 1] = 0;
}

static void append_word(char *dst, int *pos, const char *word) {
    int i = 0;

    if (*pos > 0 && *pos < LINE_MAX - 1) {
        dst[*pos] = ' ';
        *pos = *pos + 1;
    }
    while (word[i] && *pos < LINE_MAX - 1) {
        dst[*pos] = word[i];
        *pos = *pos + 1;
        i = i + 1;
    }
    dst[*pos] = 0;
}

static int wait_one(void) {
    int status = 0;
    int pid = wait(&status);

    while (pid == 0) {
        yield();
        pid = wait(&status);
    }
    return status;
}

static void exec_argv(struct Args *args) {
    char path[32];
    char argbuf[LINE_MAX];
    int pos = 0;
    int i = 1;

    if (args->argc <= 0) {
        exit(1);
    }
    make_path(arg_at(args, 0), path);
    argbuf[0] = 0;
    while (i < args->argc) {
        char *w = arg_at(args, i);

        if (streq(w, "<") || streq(w, ">")) {
            i = i + 2;
            continue;
        }
        append_word(argbuf, &pos, w);
        i = i + 1;
    }
    exec(path, argbuf);
    exit(1);
}

static void apply_redirs(struct Args *args) {
    int i = 0;

    while (i < args->argc) {
        char *w = arg_at(args, i);

        if (streq(w, "<") && i + 1 < args->argc) {
            int fd = open(arg_at(args, i + 1));
            if (fd < 0 || dup2(fd, 0) < 0) {
                exit(1);
            }
            close(fd);
            i = i + 2;
            continue;
        }
        if (streq(w, ">") && i + 1 < args->argc) {
            int fd = create(arg_at(args, i + 1));
            if (fd < 0 || dup2(fd, 1) < 0) {
                exit(1);
            }
            close(fd);
            i = i + 2;
            continue;
        }
        i = i + 1;
    }
}

static int run_simple(char *cmdline) {
    struct Args args;
    int pid;

    trim_right(cmdline);
    cmdline = skip_spaces(cmdline);
    if (!cmdline[0]) {
        return 0;
    }
    if (streq(cmdline, "exit")) {
        exit(0);
    }
    args.parse(cmdline);
    if (args.empty()) {
        return 0;
    }
    if (streq(args.get(0), "exit")) {
        exit(0);
    }

    pid = fork();
    if (pid < 0) {
        puts("?\n");
        return 1;
    }
    if (pid == 0) {
        apply_redirs(&args);
        exec_argv(&args);
    }
    return wait_one();
}

static int run_pipeline(char *left, char *right) {
    struct Args largs;
    struct Args rargs;
    int fds[2];
    int lpid;
    int rpid;

    trim_right(left);
    trim_right(right);
    left = skip_spaces(left);
    right = skip_spaces(right);
    if (!left[0] || !right[0]) {
        puts("?\n");
        return 1;
    }
    largs.parse(left);
    rargs.parse(right);
    if (pipe(fds) < 0) {
        puts("?\n");
        return 1;
    }

    lpid = fork();
    if (lpid < 0) {
        close(fds[0]);
        close(fds[1]);
        puts("?\n");
        return 1;
    }
    if (lpid == 0) {
        dup2(fds[1], 1);
        close(fds[0]);
        close(fds[1]);
        apply_redirs(&largs);
        exec_argv(&largs);
    }

    rpid = fork();
    if (rpid < 0) {
        close(fds[0]);
        close(fds[1]);
        puts("?\n");
        return 1;
    }
    if (rpid == 0) {
        dup2(fds[0], 0);
        close(fds[0]);
        close(fds[1]);
        apply_redirs(&rargs);
        exec_argv(&rargs);
    }

    close(fds[0]);
    close(fds[1]);
    wait_one();
    return wait_one();
}

static int run_line(char *line) {
    char *bar = (char *)0;
    char *andand = (char *)0;
    int i = 0;

    while (line[i]) {
        if (line[i] == '|' && bar == 0) {
            bar = line + i;
        }
        if (line[i] == '&' && line[i + 1] == '&' && andand == 0) {
            andand = line + i;
        }
        i = i + 1;
    }

    if (andand != 0) {
        int status;

        andand[0] = 0;
        andand[1] = 0;
        status = run_line(line);
        if (status == 0) {
            return run_line(andand + 2);
        }
        return status;
    }
    if (bar != 0) {
        bar[0] = 0;
        return run_pipeline(line, bar + 1);
    }
    return run_simple(line);
}

static int applet_sh(int argc, char *argv[]) {
    char line[LINE_MAX];

    while (1) {
        int n;

        puts("$ ");
        n = read_line(line, LINE_MAX);
        if (n == 0) {
            exit(2);
        }
        run_line(line);
    }
    return 0;
}

static int applet_echo(int argc, char *argv[]) {
    int i = 0;

    while (i < argc) {
        if (i > 0) {
            puts(" ");
        }
        puts(argv[i]);
        i = i + 1;
    }
    puts("\n");
    return 0;
}

static int cat_fd(int fd) {
    char buf[64];

    while (1) {
        int n = read(fd, buf, 64);

        if (n < 0) {
            return 1;
        }
        if (n == 0) {
            return 0;
        }
        write(1, buf, n);
    }
}

static int applet_cat(int argc, char *argv[]) {
    int status = 0;

    if (argc == 0) {
        return cat_fd(0);
    }
    for (int i = 0; i < argc; i++) {
        int fd = open(argv[i]);

        if (fd < 0) {
            status = 1;
        } else {
            if (cat_fd(fd) != 0) {
                status = 1;
            }
            close(fd);
        }
    }
    return status;
}

static int applet_ls(int argc, char *argv[]) {
    char name[40];
    int i = 0;

    while (1) {
        int r = dirent(i, name, 40);

        if (r < 0) {
            return 1;
        }
        if (r == 0) {
            return 0;
        }
        puts(name);
        puts("\n");
        i = i + 1;
    }
}

static int applet_mkdir(int argc, char *argv[]) {
    int status = 0;

    for (int i = 0; i < argc; i++) {
        if (mkdir(argv[i]) < 0) {
            status = 1;
        }
    }
    return status;
}

static int applet_rm(int argc, char *argv[]) {
    int status = 0;

    for (int i = 0; i < argc; i++) {
        if (rm(argv[i]) < 0) {
            status = 1;
        }
    }
    return status;
}

static int applet_pwd(int argc, char *argv[]) {
    puts("/\n");
    return 0;
}

static int wc_fd(int fd, const char *name) {
    char buf[64];
    int lines = 0;
    int words = 0;
    int bytes = 0;
    int inword = 0;

    while (1) {
        int n = read(fd, buf, 64);

        if (n < 0) {
            return 1;
        }
        if (n == 0) {
            break;
        }
        for (int i = 0; i < n; i++) {
            char c = buf[i];

            bytes = bytes + 1;
            if (c == '\n') {
                lines = lines + 1;
            }
            if (c == ' ' || c == '\n' || c == '\t') {
                inword = 0;
            } else if (!inword) {
                words = words + 1;
                inword = 1;
            }
        }
    }
    print_int(lines);
    puts(" ");
    print_int(words);
    puts(" ");
    print_int(bytes);
    if (name != 0) {
        puts(" ");
        puts(name);
    }
    puts("\n");
    return 0;
}

static int applet_wc(int argc, char *argv[]) {
    int status = 0;

    if (argc == 0) {
        return wc_fd(0, (const char *)0);
    }
    for (int i = 0; i < argc; i++) {
        int fd = open(argv[i]);

        if (fd < 0) {
            status = 1;
        } else {
            if (wc_fd(fd, argv[i]) != 0) {
                status = 1;
            }
            close(fd);
        }
    }
    return status;
}

static void grep_line(const char *pattern, char *line) {
    if (contains(line, pattern)) {
        puts(line);
        puts("\n");
    }
}

static int grep_fd(int fd, const char *pattern) {
    char buf[64];
    char line[96];
    int pos = 0;

    while (1) {
        int n = read(fd, buf, 64);

        if (n < 0) {
            return 1;
        }
        if (n == 0) {
            break;
        }
        for (int i = 0; i < n; i++) {
            char c = buf[i];

            if (c == '\n') {
                line[pos] = 0;
                grep_line(pattern, line);
                pos = 0;
            } else if (pos < 95) {
                line[pos] = c;
                pos = pos + 1;
            }
        }
    }
    if (pos > 0) {
        line[pos] = 0;
        grep_line(pattern, line);
    }
    return 0;
}

static int applet_grep(int argc, char *argv[]) {
    const char *pattern;
    int status = 0;

    if (argc < 1) {
        return 1;
    }
    pattern = argv[0];
    if (argc == 1) {
        return grep_fd(0, pattern);
    }
    for (int i = 1; i < argc; i++) {
        int fd = open(argv[i]);

        if (fd < 0) {
            status = 1;
        } else {
            if (grep_fd(fd, pattern) != 0) {
                status = 1;
            }
            close(fd);
        }
    }
    return status;
}

static int applet_ps(int argc, char *argv[]) {
    char line[64];
    int i = 0;

    while (1) {
        int r = psline(i, line, 64);

        if (r < 0) {
            return 1;
        }
        if (r == 0) {
            return 0;
        }
        puts(line);
        i = i + 1;
    }
}

static int applet_kill(int argc, char *argv[]) {
    int status = 0;

    for (int i = 0; i < argc; i++) {
        if (kill(parse_int(argv[i])) < 0) {
            status = 1;
        }
    }
    return status;
}

static int applet_exit(int argc, char *argv[]) {
    exit(0);
    return 0;
}

static int dispatch(const char *name, int argc, char *argv[]) {
    if (streq(name, "sh")) {
        return applet_sh(argc, argv);
    }
    if (streq(name, "echo")) {
        return applet_echo(argc, argv);
    }
    if (streq(name, "cat")) {
        return applet_cat(argc, argv);
    }
    if (streq(name, "ls")) {
        return applet_ls(argc, argv);
    }
    if (streq(name, "mkdir")) {
        return applet_mkdir(argc, argv);
    }
    if (streq(name, "rm")) {
        return applet_rm(argc, argv);
    }
    if (streq(name, "pwd")) {
        return applet_pwd(argc, argv);
    }
    if (streq(name, "wc")) {
        return applet_wc(argc, argv);
    }
    if (streq(name, "grep")) {
        return applet_grep(argc, argv);
    }
    if (streq(name, "ps")) {
        return applet_ps(argc, argv);
    }
    if (streq(name, "kill")) {
        return applet_kill(argc, argv);
    }
    if (streq(name, "exit")) {
        return applet_exit(argc, argv);
    }
    puts("unknown applet\n");
    return 1;
}

int main(void) {
    struct Args args;
    char argbuf[LINE_MAX];
    char *src = user_arg();
    char *name = user_argv0();
    int i = 0;
    int status;

    while (src[i] && i < LINE_MAX - 1) {
        argbuf[i] = src[i];
        i = i + 1;
    }
    argbuf[i] = 0;
    args.parse(argbuf);

    if (streq(name, "busybox") && args.argc > 0) {
        char *applet = args.shift();
        status = dispatch(applet, args.argc, args.argv);
    } else {
        status = dispatch(name, args.argc, args.argv);
    }

    exit(status);
    return status;
}

void _start(void) {
    main();
    exit(0);
}
