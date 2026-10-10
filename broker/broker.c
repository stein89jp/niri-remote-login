#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <ftw.h>
#include <grp.h>
#include <limits.h>
#include <poll.h>
#include <pwd.h>
#include <security/pam_appl.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <systemd/sd-bus.h>
#include <systemd/sd-login.h>
#include <time.h>
#include <unistd.h>

#define ROOT "/run/niri-rdp"
#ifndef WORKER
#define WORKER "/usr/lib/niri-rdp/session-worker"
#endif
#define MAX_USER 64
#define MAX_PASSWORD 512
#define MAX_JOBS 8

struct request {
    char user[MAX_USER + 1]; char password[MAX_PASSWORD + 1];
    unsigned int password_prompts, username_prompts;
};
struct session { pam_handle_t *pam; bool credentials; bool opened; pid_t worker; };
static volatile sig_atomic_t stopping;
static volatile sig_atomic_t expired;
static void stop_signal(int signo) { (void)signo; stopping = 1; }
static void expire_signal(int signo) { (void)signo; expired = 1; }
static void wipe_request(struct request *r) { explicit_bzero(r, sizeof(*r)); }

static int read_full(int fd, void *data, size_t len) {
    char *p = data;
    while (len) {
        ssize_t n = read(fd, p, len);
        if (n <= 0 || expired || stopping) return -1;
        p += n; len -= (size_t)n;
    }
    return 0;
}
static int read_request(int fd, struct request *r) {
    unsigned char h[8];
    memset(r, 0, sizeof(*r));
    if (read_full(fd, h, sizeof(h)) || memcmp(h, "NRB1", 4)) return -1;
    size_t u = ((size_t)h[4] << 8) | h[5];
    size_t p = ((size_t)h[6] << 8) | h[7];
    if (!u || u > MAX_USER || !p || p > MAX_PASSWORD) return -1;
    if (read_full(fd, r->user, u) || read_full(fd, r->password, p)) return -1;
    if (memchr(r->user, 0, u) || memchr(r->password, 0, p)) return -1;
    // Linux account names, not domain-qualified names, paths or PAM options.
    for (size_t i = 0; i < u; i++) {
        unsigned char c = (unsigned char)r->user[i];
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
              (c >= '0' && c <= '9') || c == '_' || c == '-' || c == '.')) return -1;
    }
    return 0;
}
static int respond(int fd, unsigned char status, int desktop) {
    unsigned char reply[5] = {'N','R','B','1',status};
    struct iovec iov = {.iov_base = reply, .iov_len = sizeof(reply)};
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } control;
    memset(&control, 0, sizeof(control));
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
    if (desktop >= 0) {
        msg.msg_control = control.bytes; msg.msg_controllen = sizeof(control.bytes);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &desktop, sizeof(desktop));
    }
    return sendmsg(fd, &msg, MSG_NOSIGNAL) == (ssize_t)sizeof(reply) ? 0 : -1;
}
static int conversation(int count, const struct pam_message **messages,
                        struct pam_response **responses, void *data) {
    struct request *r = data;
    if (count < 1 || count > 8) return PAM_CONV_ERR;
    struct pam_response *out = calloc((size_t)count, sizeof(*out));
    if (!out) return PAM_BUF_ERR;
    for (int i = 0; i < count; i++) {
        if (!messages[i]) goto fail;
        switch (messages[i]->msg_style) {
        case PAM_PROMPT_ECHO_OFF:
            if (r->password_prompts++) goto fail;
            out[i].resp = strdup(r->password); break;
        case PAM_PROMPT_ECHO_ON:
            if (r->username_prompts++) goto fail;
            out[i].resp = strdup(r->user); break;
        case PAM_TEXT_INFO: case PAM_ERROR_MSG: continue;
        default: goto fail;
        }
        if (!out[i].resp) goto fail;
    }
    *responses = out; return PAM_SUCCESS;
fail:
    for (int i = 0; i < count; i++) if (out[i].resp) {
        explicit_bzero(out[i].resp, strlen(out[i].resp)); free(out[i].resp);
    }
    free(out); return PAM_CONV_ERR;
}
static void cleanup(struct session *s) {
    if (s->worker > 0) {
        kill(-s->worker, SIGTERM);
        for (unsigned int i = 0; i < 20; i++) {
            int status;
            if (waitpid(s->worker, &status, WNOHANG) == s->worker) break;
            usleep(100000);
        }
        // Descendants can outlive their group leader.
        kill(-s->worker, SIGKILL);
        while (waitpid(s->worker, NULL, 0) < 0 && errno == EINTR) {}
        s->worker = 0;
    }
    if (s->pam) {
        if (s->opened) pam_close_session(s->pam, 0);
        if (s->credentials) pam_setcred(s->pam, PAM_DELETE_CRED);
        pam_end(s->pam, PAM_SUCCESS);
        s->pam = NULL; s->opened = false; s->credentials = false;
    }
}
static bool account_allowed(const struct passwd *pw) {
    if (!pw || pw->pw_uid < 1000 || !pw->pw_dir || pw->pw_dir[0] != '/' ||
        !strcmp(pw->pw_name, "niri-rdp") || !pw->pw_shell ||
        strstr(pw->pw_shell, "nologin") || !strcmp(pw->pw_shell, "/bin/false") ||
        !strcmp(pw->pw_shell, "/usr/bin/false")) return false;
    struct group *allowed = getgrnam("niri-rdp-users");
    if (!allowed) return false;
    gid_t target = allowed->gr_gid;
    int n = 64; gid_t groups[64];
    if (getgrouplist(pw->pw_name, pw->pw_gid, groups, &n) < 0) return false;
    for (int i = 0; i < n; i++) if (groups[i] == target) return true;
    return false;
}
static int graphical_sessions(uid_t uid) {
    char **sessions = NULL;
    // Include closing sessions too: their applications may still own files/buses.
    int n = sd_uid_get_sessions(uid, -1, &sessions), found = 0;
    if (n < 0) return -1;
    for (int i = 0; i < n; i++) {
        char *type = NULL;
        if (sd_session_get_type(sessions[i], &type) < 0) found = -1;
        else if (!strcmp(type, "wayland") || !strcmp(type, "x11")) found = 1;
        free(type); free(sessions[i]);
    }
    free(sessions); return found;
}
static bool valid_id(const char *id) {
    if (!id || !*id || strlen(id) > 64) return false;
    for (const char *p = id; *p; p++) if (!( (*p >= 'a' && *p <= 'z') ||
        (*p >= 'A' && *p <= 'Z') || (*p >= '0' && *p <= '9') || *p == '-' || *p == '_')) return false;
    return true;
}
static int verify_logind(const char *id, uid_t uid) {
    char *type = NULL, *service = NULL, *seat = NULL;
    uid_t actual = 0; pid_t leader = 0;
    int ok = valid_id(id) && sd_session_get_uid(id, &actual) >= 0 && actual == uid &&
        sd_session_get_leader(id, &leader) >= 0 && leader == getpid() &&
        sd_session_is_remote(id) > 0 && sd_session_get_type(id, &type) >= 0 &&
        !strcmp(type, "wayland") && sd_session_get_service(id, &service) >= 0 &&
        !strcmp(service, "niri-rdp");
    int seat_result = sd_session_get_seat(id, &seat);
    if (seat_result != -ENODATA && !(seat_result >= 0 && (!seat || !*seat))) ok = 0;
    unsigned int vt = 0;
    int vt_result = sd_session_get_vt(id, &vt);
    if (vt_result != -ENODATA && !(vt_result >= 0 && vt == 0)) ok = 0;
    free(type); free(service); free(seat); return ok ? 0 : -1;
}
static int write_registry(const char *id, pid_t pid) {
    char path[128];
    snprintf(path, sizeof(path), ROOT "/sessions/%ld", (long)pid);
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) return -1;
    ssize_t n = write(fd, id, strlen(id));
    close(fd); return n == (ssize_t)strlen(id) ? 0 : -1;
}
static int remove_entry(const char *path, const struct stat *st, int type, struct FTW *f) {
    (void)st; (void)type; (void)f; return remove(path);
}
static void remove_desktop(const char *path, uid_t uid, gid_t gid) {
    pid_t child = fork();
    if (child == 0) {
        if (setgroups(0, NULL) || setgid(gid) || setuid(uid)) _exit(1);
        // Root-owned parent prevents replacing this path with a symlink.
        int result = nftw(path, remove_entry, 16, FTW_DEPTH | FTW_PHYS);
        _exit(result == 0 ? 0 : 1);
    }
    if (child > 0) while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
    // Only root can remove the directory from its root-owned parent.
    // An empty rmdir never follows user-controlled symlinks.
    if (child > 0) rmdir(path);
}
static void child_environment(const struct passwd *pw, const char *runtime,
                              const char *id, const char *private) {
    clearenv();
    setenv("PATH", "/usr/bin:/bin", 1); setenv("LANG", "C.UTF-8", 1);
    setenv("HOME", pw->pw_dir, 1); setenv("USER", pw->pw_name, 1);
    setenv("LOGNAME", pw->pw_name, 1); setenv("SHELL", pw->pw_shell, 1);
    setenv("XDG_RUNTIME_DIR", runtime, 1); setenv("XDG_SESSION_ID", id, 1);
    setenv("XDG_SESSION_TYPE", "wayland", 1); setenv("XDG_SESSION_CLASS", "user", 1);
    setenv("XDG_CURRENT_DESKTOP", "niri", 1); setenv("XDG_SESSION_DESKTOP", "niri", 1);
    setenv("NIRI_RDP_RUNTIME_DIR", private, 1);
}
static int launch(struct session *s, const struct passwd *pw, const char *runtime,
                  const char *id, const char *private, int control, int lock) {
    struct stat st;
    if (stat(WORKER, &st) || !S_ISREG(st.st_mode) || st.st_uid != 0 ||
        (st.st_mode & 022) || !(st.st_mode & 0111)) return -1;
    s->worker = fork();
    if (s->worker == 0) {
        close(control); close(lock);
        if (setsid() < 0 || initgroups(pw->pw_name, pw->pw_gid) ||
            setgid(pw->pw_gid) || setuid(pw->pw_uid)) _exit(125);
        child_environment(pw, runtime, id, private);
        if (chdir(pw->pw_dir)) _exit(125);
        int nullfd = open("/dev/null", O_RDONLY);
        if (nullfd < 0 || dup2(nullfd, STDIN_FILENO) < 0) _exit(125);
        if (nullfd > STDERR_FILENO) close(nullfd);
        // Descriptor numbers beyond stdio are close-on-exec or explicitly closed.
        execl("/usr/bin/dbus-run-session", "dbus-run-session", "--", WORKER, (char *)NULL);
        _exit(126);
    }
    return s->worker > 0 ? 0 : -1;
}
static int desktop_fd(const char *private, uid_t uid, gid_t gid) {
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)) return -1;
    pid_t child = fork();
    if (child == 0) {
        close(pair[0]);
        if (setgroups(0, NULL) || setgid(gid) || setuid(uid)) _exit(1);
        int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        struct sockaddr_un address = {.sun_family = AF_UNIX};
        int size = snprintf(address.sun_path, sizeof(address.sun_path), "%s/wayland-0", private);
        if (fd < 0 || size < 0 || (size_t)size >= sizeof(address.sun_path) ||
            connect(fd, (struct sockaddr *)&address, sizeof(address))) _exit(1);
        _exit(respond(pair[1], 0, fd) ? 1 : 0);
    }
    close(pair[1]); int fd = -1;
    unsigned char data[5]; struct iovec iov = {.iov_base = data, .iov_len = sizeof(data)};
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } ancillary;
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = ancillary.bytes, .msg_controllen = sizeof(ancillary.bytes)};
    if (child > 0 && recvmsg(pair[0], &msg, MSG_CMSG_CLOEXEC) == 5) {
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        if (!(msg.msg_flags & MSG_CTRUNC) && c && c->cmsg_level == SOL_SOCKET &&
            c->cmsg_type == SCM_RIGHTS && c->cmsg_len == CMSG_LEN(sizeof(int)))
            memcpy(&fd, CMSG_DATA(c), sizeof(fd));
    }
    close(pair[0]);
    if (child > 0) while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
    return fd;
}
static void handle_connection(int fd, int ready) {
    struct request r; struct session s = {0};
    int lock = -1, desktop = -1; unsigned char failure = 3;
    char private[128] = {0}; uid_t uid = 0; gid_t gid = 0;
    struct timeval timeout = {.tv_sec = 10};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    // Deadlines also bound a stalled or misconfigured PAM module.
    alarm(30);
    if (read_request(fd, &r)) goto out;
    failure = 1;
    struct pam_conv conv = {.conv = conversation, .appdata_ptr = &r};
    if (pam_start("niri-rdp", r.user, &conv, &s.pam) != PAM_SUCCESS) goto out;
    if (pam_set_item(s.pam, PAM_RHOST, "rdp-loopback") != PAM_SUCCESS ||
        pam_authenticate(s.pam, PAM_DISALLOW_NULL_AUTHTOK) != PAM_SUCCESS ||
        pam_acct_mgmt(s.pam, 0) != PAM_SUCCESS) goto out;
    const void *name = NULL;
    if (pam_get_item(s.pam, PAM_USER, &name) != PAM_SUCCESS || !name) goto out;
    struct passwd *pw = getpwnam(name);
    if (!account_allowed(pw)) goto out;
    uid = pw->pw_uid; gid = pw->pw_gid;
    char lockpath[128]; snprintf(lockpath, sizeof(lockpath), ROOT "/user-%lu.lock", (unsigned long)uid);
    lock = open(lockpath, O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (lock < 0 || flock(lock, LOCK_EX | LOCK_NB) || graphical_sessions(uid) != 0) goto out;
    failure = 2;
    if (pam_putenv(s.pam, "XDG_SESSION_TYPE=wayland") != PAM_SUCCESS ||
        pam_putenv(s.pam, "XDG_SESSION_CLASS=user") != PAM_SUCCESS ||
        pam_putenv(s.pam, "XDG_SESSION_DESKTOP=niri") != PAM_SUCCESS ||
        pam_putenv(s.pam, "XDG_SEAT=") != PAM_SUCCESS ||
        pam_putenv(s.pam, "XDG_VTNR=") != PAM_SUCCESS ||
        pam_putenv(s.pam, "DISPLAY=") != PAM_SUCCESS ||
        pam_setcred(s.pam, PAM_ESTABLISH_CRED) != PAM_SUCCESS) goto out;
    s.credentials = true;
    if (pam_open_session(s.pam, 0) != PAM_SUCCESS) goto out;
    s.opened = true;
    const char *id = pam_getenv(s.pam, "XDG_SESSION_ID");
    // Record ownership before later validation/launch can fail or be killed.
    if (!valid_id(id) || write_registry(id, getpid())) goto out;
    const char *runtime = pam_getenv(s.pam, "XDG_RUNTIME_DIR");
    char expected[64]; snprintf(expected, sizeof(expected), "/run/user/%lu", (unsigned long)uid);
    if (expired || stopping || !runtime || strcmp(expected, runtime) || verify_logind(id, uid)) goto out;
    struct stat runtime_st;
    if (lstat(runtime, &runtime_st) || !S_ISDIR(runtime_st.st_mode) || runtime_st.st_uid != uid ||
        (runtime_st.st_mode & 077)) goto out;
    snprintf(private, sizeof(private), ROOT "/desktops/%ld", (long)getpid());
    if (mkdir(private, 0700) || chown(private, uid, gid)) goto out;
    // getgrnam/getgrouplist may replace NSS storage; resolve final account again.
    pw = getpwuid(uid);
    if (!pw || launch(&s, pw, runtime, id, private, fd, lock)) goto out;
    wipe_request(&r);
    for (unsigned int i = 0; i < 150 && !stopping && !expired; i++) {
        desktop = desktop_fd(private, uid, gid);
        if (desktop >= 0) break;
        if (waitpid(s.worker, NULL, WNOHANG) == s.worker) goto out;
        usleep(100000);
    }
    if (desktop < 0 || respond(fd, 0, desktop)) goto out;
    failure = 255; // A success response must never be followed by another header.
    if (write(ready, "R", 1) != 1) goto out;
    close(ready); ready = -1;
    close(desktop); desktop = -1; alarm(0);
    while (!stopping) {
        struct pollfd p = {.fd = fd, .events = POLLIN};
        int result = poll(&p, 1, 200);
        if (result > 0 || (result < 0 && errno != EINTR) ||
            waitpid(s.worker, NULL, WNOHANG) == s.worker) break;
    }
out:
    alarm(0); wipe_request(&r);
    if (failure != 255) respond(fd, failure, -1);
    if (desktop >= 0) close(desktop);
    cleanup(&s);
    if (*private) remove_desktop(private, uid, gid);
    if (lock >= 0) close(lock);
    if (ready >= 0) close(ready);
    close(fd);
}
static int terminate_id(const char *id) {
    char *service = NULL;
    int result = sd_session_get_service(id, &service);
    if (result < 0) return (result == -ENXIO || result == -ENOENT || result == -ENODATA) ? 0 : -1;
    // Never terminate a different PAM service even if stale IDs are recycled.
    if (!strcmp(service, "niri-rdp")) {
        sd_bus *bus = NULL; sd_bus_error error = SD_BUS_ERROR_NULL;
        result = sd_bus_open_system(&bus);
        if (result >= 0) {
            sd_bus_set_method_call_timeout(bus, 5000000);
            result = sd_bus_call_method(bus, "org.freedesktop.login1", "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager", "TerminateSession", &error, NULL, "s", id);
            if (sd_bus_error_has_name(&error, "org.freedesktop.login1.NoSuchSession")) result = 0;
        }
        sd_bus_error_free(&error); sd_bus_unref(bus);
    }
    free(service);
    return result < 0 ? -1 : 0;
}
static void terminate_unrecorded(pid_t pid) {
    char **sessions = NULL;
    int n = sd_get_sessions(&sessions);
    for (int i = 0; i < n; i++) {
        pid_t leader = 0;
        char *service = NULL;
        if (sd_session_get_leader(sessions[i], &leader) >= 0 && leader > 0 &&
            (pid == 0 || leader == pid) && sd_session_get_service(sessions[i], &service) >= 0 &&
            !strcmp(service, "niri-rdp")) {
            // Recover the record if a holder died during PAM registration.
            // O_EXCL preserves an already recorded identity for this holder.
            if (valid_id(sessions[i])) write_registry(sessions[i], leader);
            terminate_id(sessions[i]);
        }
        free(service);
        free(sessions[i]);
    }
    free(sessions);
}
static void terminate_record(pid_t pid) {
    char path[128], id[65] = {0};
    snprintf(path, sizeof(path), ROOT "/sessions/%ld", (long)pid);
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    bool keep = false;
    if (fd >= 0) {
        ssize_t n = read(fd, id, sizeof(id)-1); close(fd);
        if (n > 0 && valid_id(id)) keep = terminate_id(id) < 0;
        if (!keep) unlink(path);
        else fputs("logind teardown failed; session record retained for retry\n", stderr);
    }
    // Covers SIGKILL or PAM failure between logind registration and registry write.
    terminate_unrecorded(pid);
    if (keep || access(path, F_OK) == 0) return;
    char desktop[128];
    snprintf(desktop, sizeof(desktop), ROOT "/desktops/%ld", (long)pid);
    struct stat st;
    if (lstat(desktop, &st) == 0 && S_ISDIR(st.st_mode) && st.st_uid >= 1000)
        remove_desktop(desktop, st.st_uid, st.st_gid);
}
static void cleanup_records(void) {
    DIR *dir = opendir(ROOT "/sessions"); if (!dir) { terminate_unrecorded(0); return; }
    struct dirent *entry;
    while ((entry = readdir(dir))) {
        char *end; errno = 0; long pid = strtol(entry->d_name, &end, 10);
        if (!errno && !*end && pid > 0 && pid <= INT_MAX) terminate_record((pid_t)pid);
    }
    closedir(dir);
    terminate_unrecorded(0);
}
static void retry_stale_records(void) {
    DIR *dir = opendir(ROOT "/sessions"); if (!dir) return;
    struct dirent *entry;
    while ((entry = readdir(dir))) {
        char *end; errno = 0; long pid = strtol(entry->d_name, &end, 10);
        if (!errno && !*end && pid > 0 && pid <= INT_MAX &&
            kill((pid_t)pid, 0) < 0 && errno == ESRCH) terminate_record((pid_t)pid);
    }
    closedir(dir);
}
static bool peer_allowed(int fd, uid_t uid) {
    struct ucred peer; socklen_t size = sizeof(peer);
    return getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &peer, &size) == 0 &&
        size == sizeof(peer) && peer.uid == uid;
}
#ifndef BROKER_TEST
int main(int argc, char **argv) {
    if (geteuid() != 0) { fputs("broker requires system-service root privileges\n", stderr); return 1; }
    struct rlimit core = {0}; setrlimit(RLIMIT_CORE, &core);
    prctl(PR_SET_DUMPABLE, 0); umask(077);
    if (argc == 2 && !strcmp(argv[1], "--cleanup")) { cleanup_records(); return 0; }
    if (argc != 1) return 2;
    clearenv(); setenv("PATH", "/usr/bin:/bin", 1); setenv("LANG", "C.UTF-8", 1);
    struct passwd *frontend = getpwnam("niri-rdp"); if (!frontend || frontend->pw_uid == 0) return 1;
    uid_t frontend_uid = frontend->pw_uid; gid_t frontend_gid = frontend->pw_gid;
    if ((mkdir(ROOT "/sessions", 0700) && errno != EEXIST) ||
        (mkdir(ROOT "/desktops", 0711) && errno != EEXIST)) return 1;
    // umask(077) would remove traversal from mode0711.
    if (chmod(ROOT "/desktops", 0711)) return 1;
    int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX}; strcpy(address.sun_path, ROOT "/broker.sock");
    unlink(address.sun_path);
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof(address)) ||
        chown(address.sun_path, 0, frontend_gid) || chmod(address.sun_path, 0660) || listen(listener, MAX_JOBS)) return 1;
    struct sigaction sa = {.sa_handler = stop_signal}; sigemptyset(&sa.sa_mask);
    sigaction(SIGTERM, &sa, NULL); sigaction(SIGINT, &sa, NULL); signal(SIGPIPE, SIG_IGN);
    sa.sa_handler = expire_signal; sigaction(SIGALRM, &sa, NULL);
    pid_t jobs[MAX_JOBS] = {0};
    time_t started[MAX_JOBS] = {0};
    time_t retried = time(NULL);
    int ready[MAX_JOBS]; for (int i = 0; i < MAX_JOBS; i++) ready[i] = -1;
    while (!stopping) {
        if (time(NULL) - retried >= 5) { retry_stale_records(); retried = time(NULL); }
        for (int i = 0; i < MAX_JOBS; i++) if (jobs[i] && waitpid(jobs[i], NULL, WNOHANG) == jobs[i]) {
            terminate_record(jobs[i]); jobs[i] = 0; if (ready[i] >= 0) close(ready[i]); ready[i] = -1;
        }
        for (int i = 0; i < MAX_JOBS; i++) if (jobs[i] && ready[i] >= 0) {
            char byte;
            if (read(ready[i], &byte, 1) == 1 && byte == 'R') { close(ready[i]); ready[i] = -1; }
            else if (time(NULL) - started[i] >= 35) {
                // Bound PAM even if its module ignores interrupted I/O.
                kill(jobs[i], SIGKILL);
            }
        }
        struct pollfd p = {.fd = listener, .events = POLLIN};
        if (poll(&p, 1, 200) <= 0) continue;
        int fd = accept4(listener, NULL, NULL, SOCK_CLOEXEC); if (fd < 0) continue;
        int slot = -1; for (int i = 0; i < MAX_JOBS; i++) if (!jobs[i]) { slot = i; break; }
        if (slot < 0 || !peer_allowed(fd, frontend_uid)) {
            respond(fd, 3, -1); close(fd); continue;
        }
        int notification[2];
        if (pipe2(notification, O_CLOEXEC | O_NONBLOCK)) { close(fd); continue; }
        pid_t pid = fork();
        if (pid == 0) {
            close(listener); stopping = 0;
            close(notification[0]);
            for (int i = 0; i < MAX_JOBS; i++) if (ready[i] >= 0) close(ready[i]);
            if (setsid() < 0) _exit(1);
            // A service SIGKILL also tears down session holders via parent death.
            prctl(PR_SET_PDEATHSIG, SIGTERM);
            if (getppid() == 1) _exit(1);
            handle_connection(fd, notification[1]); _exit(0);
        }
        close(notification[1]);
        if (pid > 0) { jobs[slot] = pid; started[slot] = time(NULL); ready[slot] = notification[0]; }
        else close(notification[0]);
        close(fd);
    }
    close(listener); unlink(address.sun_path);
    for (int i = 0; i < MAX_JOBS; i++) if (jobs[i]) kill(jobs[i], SIGTERM);
    for (int i = 0; i < MAX_JOBS; i++) if (jobs[i]) {
        while (waitpid(jobs[i], NULL, 0) < 0 && errno == EINTR) {}
        terminate_record(jobs[i]);
    }
    cleanup_records(); return 0;
}
#endif
