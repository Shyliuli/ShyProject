#![no_main]
#![mem(1M)]
#![stack(4K)]

#include "usys.h"
#include "ulib.h"

static char *skip_spaces(char *s) {
    while (*s == ' ') {
        s = s + 1;
    }
    return s;
}

static char *trim_right(char *s) {
    int n = 0;

    while (s[n]) {
        n = n + 1;
    }
    while (n > 0 && s[n - 1] == ' ') {
        n = n - 1;
        s[n] = 0;
    }
    return s;
}

static const char *command_path(char *cmd) {
    if (streq(cmd, "echo")) {
        return "/echo";
    }
    if (streq(cmd, "cat")) {
        return "/cat";
    }
    if (streq(cmd, "stackbig")) {
        return "/stackbig";
    }
    if (streq(cmd, "p2wait")) {
        return "/p2wait";
    }
    if (streq(cmd, "p2wake")) {
        return "/p2wake";
    }
    if (streq(cmd, "p2kill")) {
        return "/p2kill";
    }
    if (streq(cmd, "p3lock")) {
        return "/p3lock";
    }
    if (streq(cmd, "p4pipe")) {
        return "/p4pipe";
    }
    return (const char *)0;
}

static const char *parse_command(char *s, char **arg) {
    char *cmd;
    int i = 0;

    cmd = skip_spaces(s);
    trim_right(cmd);
    while (cmd[i] && cmd[i] != ' ') {
        i = i + 1;
    }
    if (cmd[i]) {
        cmd[i] = 0;
        *arg = skip_spaces(cmd + i + 1);
    } else {
        *arg = cmd + i;
    }
    return command_path(cmd);
}

static void wait_for_children(int want) {
    int done = 0;

    while (done < want) {
        int pid = wait((int *)0);
        if (pid > 0) {
            done = done + 1;
        } else if (pid == 0) {
            yield();
        } else {
            return;
        }
    }
}

static void run_program(const char *path, char *arg) {
    int pid = fork();

    if (pid < 0) {
        puts("?\n");
        return;
    }
    if (pid == 0) {
        exec(path, arg);
        exit(1);
    }
    wait_for_children(1);
}

static void run_pipeline(char *left, char *right) {
    char *left_arg;
    char *right_arg;
    const char *left_path;
    const char *right_path;
    int fds[2];
    int left_pid;
    int right_pid;

    left_path = parse_command(left, &left_arg);
    right_path = parse_command(right, &right_arg);
    if (left_path == 0 || right_path == 0) {
        puts("?\n");
        return;
    }
    if (pipe(fds) < 0) {
        puts("?\n");
        return;
    }

    left_pid = fork();
    if (left_pid < 0) {
        close(fds[0]);
        close(fds[1]);
        puts("?\n");
        return;
    }
    if (left_pid == 0) {
        dup2(fds[1], 1);
        close(fds[0]);
        close(fds[1]);
        exec(left_path, left_arg);
        exit(1);
    }

    right_pid = fork();
    if (right_pid < 0) {
        close(fds[0]);
        close(fds[1]);
        puts("?\n");
        return;
    }
    if (right_pid == 0) {
        dup2(fds[0], 0);
        close(fds[0]);
        close(fds[1]);
        exec(right_path, right_arg);
        exit(1);
    }

    close(fds[0]);
    close(fds[1]);
    wait_for_children(2);
}

int main(void) {
    char line[96];

    while (1) {
        char *cmd;
        char *arg;
        char *bar;
        const char *path;
        int i;
        int n;

        puts("$ ");
        n = read_line(line, 96);
        if (n == 0) {
            exit(2);
        }

        cmd = skip_spaces(line);
        if (!cmd[0]) {
            continue;
        }

        bar = (char *)0;
        i = 0;
        while (cmd[i]) {
            if (cmd[i] == '|') {
                bar = cmd + i;
                break;
            }
            i = i + 1;
        }
        if (bar != 0) {
            bar[0] = 0;
            run_pipeline(cmd, bar + 1);
            continue;
        }

        i = 0;
        while (cmd[i] && cmd[i] != ' ') {
            i = i + 1;
        }
        if (cmd[i]) {
            cmd[i] = 0;
            arg = skip_spaces(cmd + i + 1);
        } else {
            arg = cmd + i;
        }

        if (streq(cmd, "exit")) {
            exit(0);
        }

        path = command_path(cmd);
        if (path != 0) {
            run_program(path, arg);
        } else {
            puts("?\n");
        }
    }

    exit(0);
    return 0;
}

void _start(void) {
    main();
    exit(0);
}
