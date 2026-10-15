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
#include <syslog.h>
#include <systemd/sd-bus.h>
#include <systemd/sd-journal.h>
#include <systemd/sd-login.h>
#include <time.h>
#include <unistd.h>

#ifndef ROOT
#define ROOT "/run/niri-rdp"
#endif
#ifndef WORKER
#define WORKER "/usr/lib/niri-rdp/session-worker"
#endif
#ifndef CONFIG
#define CONFIG "/etc/niri-rdp/broker.conf"
#endif
#ifndef AUDIT_SINK
#define AUDIT_SINK journal_audit
#endif
// Wire contract v2: the frontend and broker are upgraded together.
#define MAGIC "NRB2"
#define MAX_USER 64
#define MAX_PASSWORD 512
#define MAX_JOBS 8
#define MAX_RHOST INET6_ADDRSTRLEN
#define MAX_CONFIG 4096
#define MAX_GRACE 3600
// Processes in a logged-out session get this long to exit after SIGTERM.
#define TERM_GRACE_TICKS 20

// Codes other than REJECTED are sent only after PAM and the broker's own
// group policy accepted the account, so they reveal nothing to a guesser.
enum status {
    STATUS_OK = 0,
    STATUS_REJECTED = 1,        // any authentication, account or policy failure
    STATUS_UNAVAILABLE = 2,     // session startup failed or is in transition
    STATUS_INVALID = 3,         // malformed/unauthorized request or no capacity
    STATUS_ATTACHED = 4,        // the account's desktop serves another connection
    STATUS_LOCAL_SESSION = 5,   // the account has a local graphical session
    STATUS_NONE = 255,          // a reply was already sent
};
enum session_end { END_STOPPING, END_DESKTOP_EXIT, END_GRACE_EXPIRED, END_ERROR };
struct config { unsigned int grace_seconds; };
static const struct config default_config = {.grace_seconds = 45};
static struct config settings = {.grace_seconds = 45};

struct request {
    char user[MAX_USER + 1]; char password[MAX_PASSWORD + 1];
    char rhost[MAX_RHOST];
    unsigned int password_prompts, username_prompts;
};
struct session {
    pam_handle_t *pam; bool credentials; bool opened; pid_t worker;
    // umask and core limit chosen by the PAM session (pam_umask, pam_limits).
    bool limits; mode_t umask; struct rlimit core;
    // Audit identity: final PAM account, client address and logind session.
    char user[MAX_USER + 1]; char rhost[MAX_RHOST]; char id[65];
};
static volatile sig_atomic_t stopping;
static volatile sig_atomic_t expired;
static void stop_signal(int signo) { (void)signo; stopping = 1; }
static void expire_signal(int signo) { (void)signo; expired = 1; }
static void wipe_request(struct request *r) { explicit_bzero(r, sizeof(*r)); }

// Structured journal entry for administrators and fail2ban. Never pass a
// password: only the event, account, client address, outcome and logind ID.
// User names are restricted to [A-Za-z0-9_.-], so fields cannot be spoofed.
static void journal_audit(const char *event, const char *result, const char *user,
                          const char *rhost, const char *id, int priority) {
    sd_journal_send("MESSAGE=%s user=%s rhost=%s result=%s session=%s",
        event, user, rhost, result, id,
        "PRIORITY=%i", priority, "SYSLOG_IDENTIFIER=niri-rdp-broker",
        "NIRI_RDP_EVENT=%s", event, "NIRI_RDP_USER=%s", user, "NIRI_RDP_RHOST=%s", rhost,
        "NIRI_RDP_RESULT=%s", result, "NIRI_RDP_SESSION=%s", id, NULL);
}
static const char *or_dash(const char *value) { return value && *value ? value : "-"; }
static void audit(const char *event, const char *result, const char *user,
                  const char *rhost, const char *id, int priority) {
    AUDIT_SINK(event, result, or_dash(user), or_dash(rhost), or_dash(id), priority);
}
static const char *status_result(unsigned char status) {
    switch (status) {
    case STATUS_OK: return "success";
    case STATUS_REJECTED: return "rejected";
    case STATUS_INVALID: return "invalid";
    case STATUS_ATTACHED: return "already-attached";
    case STATUS_LOCAL_SESSION: return "local-session";
    default: return "unavailable";
    }
}
static const char *end_result(enum session_end end) {
    switch (end) {
    case END_STOPPING: return "broker-stopping";
    case END_DESKTOP_EXIT: return "desktop-exited";
    case END_GRACE_EXPIRED: return "grace-expired";
    default: return "error";
    }
}

static bool config_space(char c) { return c == ' ' || c == '\t'; }
// Strict "key = value" lines; blank lines and lines starting with '#' are
// ignored. Unknown or repeated keys, signs, suffixes and out-of-range values
// reject the whole file.
static int parse_config(char *text, struct config *out, const char **why) {
    struct config parsed = default_config;
    bool grace = false;
    for (char *line = text, *next; line; line = next) {
        next = strchr(line, '\n');
        if (next) *next++ = 0;
        while (config_space(*line)) line++;
        if (!*line || *line == '#') continue;
        size_t k = 0;
        while ((line[k] >= 'a' && line[k] <= 'z') || line[k] == '_') k++;
        char *p = line + k;
        while (config_space(*p)) p++;
        if (!k || *p != '=') { *why = "malformed line"; return -1; }
        line[k] = 0;
        for (p++; config_space(*p); p++) {}
        char *value = p; size_t v = 0;
        while (value[v] >= '0' && value[v] <= '9') v++;
        for (p = value + v; config_space(*p); p++) {}
        if (!v || *p) { *why = "value is not a decimal number"; return -1; }
        if (strcmp(line, "grace_seconds")) { *why = "unknown key"; return -1; }
        if (grace) { *why = "repeated key"; return -1; }
        unsigned long number = v > 4 ? MAX_GRACE + 1UL : strtoul(value, NULL, 10);
        if (number > MAX_GRACE) { *why = "grace_seconds must be 0-3600"; return -1; }
        parsed.grace_seconds = (unsigned int)number; grace = true;
    }
    *out = parsed; return 0;
}
// Returns 0 when loaded, 1 when absent and -1 when rejected. Every outcome
// except 0 leaves the defaults: the broker logs a rejected file and keeps
// running, because a typo must not lock out the users of a remote-only host.
static int load_config(const char *path, uid_t owner, struct config *out, const char **why) {
    *out = default_config; *why = NULL;
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NOCTTY | O_NONBLOCK);
    if (fd < 0) {
        if (errno == ENOENT) return 1;
        *why = errno == ELOOP ? "symbolic link" : "cannot open"; return -1;
    }
    struct stat st; char text[MAX_CONFIG + 2]; size_t size = 0;
    if (fstat(fd, &st)) *why = "cannot stat";
    else if (!S_ISREG(st.st_mode)) *why = "not a regular file";
    else if (st.st_uid != owner) *why = "wrong owner";
    else if (st.st_mode & 022) *why = "writable by group or others";
    else for (;;) {
        ssize_t n = read(fd, text + size, sizeof(text) - 1 - size);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) { *why = "cannot read"; break; }
        if (n == 0) break;
        size += (size_t)n;
        if (size > MAX_CONFIG) { *why = "larger than 4096 bytes"; break; }
    }
    close(fd);
    if (*why) return -1;
    text[size] = 0;
    if (memchr(text, 0, size)) { *why = "contains a NUL byte"; return -1; }
    struct config parsed;
    if (parse_config(text, &parsed, why)) return -1;
    *out = parsed; return 0;
}

static int read_full(int fd, void *data, size_t len) {
    char *p = data;
    while (len) {
        ssize_t n = read(fd, p, len);
        if (n <= 0 || expired || stopping) return -1;
        p += n; len -= (size_t)n;
    }
    return 0;
}
// Request: magic, user length, password length (both u16 big endian), the
// client address family (0 unknown, 4 IPv4, 6 IPv6), 0/4/16 address bytes,
// then the user name and password.
static int read_request(int fd, struct request *r) {
    unsigned char h[9], address[16];
    memset(r, 0, sizeof(*r));
    if (read_full(fd, h, sizeof(h)) || memcmp(h, MAGIC, 4)) return -1;
    size_t u = ((size_t)h[4] << 8) | h[5];
    size_t p = ((size_t)h[6] << 8) | h[7];
    if (!u || u > MAX_USER || !p || p > MAX_PASSWORD) return -1;
    if (h[8] != 0 && h[8] != 4 && h[8] != 6) return -1;
    size_t a = h[8] == 4 ? 4 : h[8] == 6 ? 16 : 0;
    if (a && (read_full(fd, address, a) ||
              !inet_ntop(a == 4 ? AF_INET : AF_INET6, address, r->rhost, sizeof(r->rhost))))
        return -1;
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
static int send_fd(int fd, void *data, size_t size, int desktop) {
    struct iovec iov = {.iov_base = data, .iov_len = size};
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } control;
    memset(&control, 0, sizeof(control));
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
    if (desktop >= 0) {
        msg.msg_control = control.bytes; msg.msg_controllen = sizeof(control.bytes);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &desktop, sizeof(desktop));
    }
    return sendmsg(fd, &msg, MSG_NOSIGNAL) == (ssize_t)size ? 0 : -1;
}
static int respond(int fd, unsigned char status, int desktop) {
    unsigned char reply[5] = {'N','R','B','2',status};
    return send_fd(fd, reply, sizeof(reply), desktop);
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
// True while any member of the process group exists, zombies included.
static bool group_alive(pid_t group) { return kill(-group, 0) == 0 || errno == EPERM; }
static void cleanup(struct session *s) {
    if (s->worker > 0) {
        kill(-s->worker, SIGTERM);
        // Descendants can outlive their group leader, which the session holder
        // may already have reaped. Give the whole group the same grace period
        // to save state after SIGTERM before forcing it.
        bool reaped = false;
        for (unsigned int i = 0; i < TERM_GRACE_TICKS; i++) {
            if (!reaped) {
                pid_t result = waitpid(s->worker, NULL, WNOHANG);
                reaped = result == s->worker || (result < 0 && errno == ECHILD);
            }
            if (reaped && !group_alive(s->worker)) break;
            usleep(100000);
        }
        kill(-s->worker, SIGKILL);
        if (!reaped) while (waitpid(s->worker, NULL, 0) < 0 && errno == EINTR) {}
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
    gid_t *list = groups;
    if (getgrouplist(pw->pw_name, pw->pw_gid, groups, &n) < 0) {
        // A small buffer is not a policy rejection. NSS reports the required
        // count so accounts with many supplementary groups can be retried.
        if (n <= 64 || (size_t)n > SIZE_MAX / sizeof(*list)) return false;
        list = malloc((size_t)n * sizeof(*list));
        if (!list) return false;
        if (getgrouplist(pw->pw_name, pw->pw_gid, list, &n) < 0) {
            free(list); return false;
        }
    }
    bool found = false;
    for (int i = 0; i < n; i++) if (list[i] == target) { found = true; break; }
    if (list != groups) free(list);
    return found;
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
// pam_umask and pam_limits act on the calling process during
// pam_open_session. Offer the usual login defaults (umask 022, core limit
// 0/unlimited), keep what the PAM session chooses for the desktop, and return
// the broker itself to its own umask and core limit.
static int open_pam_session(struct session *s) {
    struct rlimit broker_core;
    bool restore = getrlimit(RLIMIT_CORE, &broker_core) == 0;
    struct rlimit offered = {.rlim_cur = 0, .rlim_max = RLIM_INFINITY};
    setrlimit(RLIMIT_CORE, &offered); // Fails harmlessly without privilege.
    mode_t broker_mask = umask(022);
    int result = pam_open_session(s->pam, 0);
    s->umask = umask(broker_mask);
    s->limits = getrlimit(RLIMIT_CORE, &s->core) == 0;
    if (restore) setrlimit(RLIMIT_CORE, &broker_core);
    return result;
}
// Desktop processes must not inherit the broker's UMask=0077 and core limit
// 0/0: apply the PAM session's choice, or 022 and 0/unlimited without one.
static void session_process_limits(const struct session *s) {
    umask(s->limits ? s->umask & 0777 : 022);
    struct rlimit core = {.rlim_cur = 0, .rlim_max = RLIM_INFINITY};
    if (s->limits) core = s->core;
    if (setrlimit(RLIMIT_CORE, &core)) {
        // Without privilege the hard limit cannot be raised: keep it.
        struct rlimit current;
        if (getrlimit(RLIMIT_CORE, &current)) return;
        core.rlim_max = current.rlim_max;
        if (core.rlim_cur > core.rlim_max) core.rlim_cur = core.rlim_max;
        setrlimit(RLIMIT_CORE, &core);
    }
}
static int launch(struct session *s, const struct passwd *pw, const char *runtime,
                  const char *id, const char *private, int control, int lock) {
    struct stat st;
    if (stat(WORKER, &st) || !S_ISREG(st.st_mode) || st.st_uid != 0 ||
        (st.st_mode & 022) || !(st.st_mode & 0111)) return -1;
    s->worker = fork();
    if (s->worker == 0) {
        close(control); close(lock);
        session_process_limits(s);
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
static int session_socket_fd(const char *private, uid_t uid, gid_t gid, const char *name) {
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)) return -1;
    pid_t child = fork();
    if (child == 0) {
        close(pair[0]);
        if (setgroups(0, NULL) || setgid(gid) || setuid(uid)) _exit(1);
        int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        struct sockaddr_un address = {.sun_family = AF_UNIX};
        int size = snprintf(address.sun_path, sizeof(address.sun_path), "%s/%s", private, name);
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
static int desktop_fd(const char *private, uid_t uid, gid_t gid) {
    return session_socket_fd(private, uid, gid, "wayland-0");
}
// Ordered capabilities: compositor, then optional versioned private PCM,
// then the optional remote microphone (NRM1). Position alone identifies each:
// the microphone is sent only together with playback, so [desktop, audio] and
// [desktop] stay exactly as before. Both are ancillary: an unavailable helper
// cannot reject an authenticated resume of a healthy desktop, and a microphone
// failure never removes playback. A later reconnect receives fresh descriptors.
static int respond_session(int fd, int desktop, const char *private, uid_t uid, gid_t gid) {
    int audio = session_socket_fd(private, uid, gid, "audio-0");
    if (audio < 0) return respond(fd, 0, desktop);
    int microphone = session_socket_fd(private, uid, gid, "audio-in-0");
    int descriptors[3] = {desktop, audio, microphone};
    size_t count = microphone < 0 ? 2 : 3;
    unsigned char reply[5] = {'N','R','B','2',0};
    struct iovec iov = {.iov_base=reply, .iov_len=sizeof(reply)};
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(descriptors))]; } control;
    memset(&control, 0, sizeof(control));
    struct msghdr msg = {.msg_iov=&iov, .msg_iovlen=1,
        .msg_control=control.bytes, .msg_controllen=CMSG_SPACE(count * sizeof(int))};
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(count * sizeof(int));
    memcpy(CMSG_DATA(c), descriptors, count * sizeof(int));
    int result = sendmsg(fd, &msg, MSG_NOSIGNAL) == (ssize_t)sizeof(reply) ? 0 : -1;
    close(audio);
    if (microphone >= 0) close(microphone);
    return result;
}
// Root-only handoff channel: only an already PAM-authorized broker child
// can route a fresh frontend control stream to the holder of this UID. The
// message carries the magic, a zero status and the client address text.
static int receive_handoff(int channel, char rhost[MAX_RHOST]) {
    char data[5 + MAX_RHOST];
    struct iovec iov = {.iov_base=data, .iov_len=sizeof(data)};
    union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } control;
    struct msghdr msg = {.msg_iov=&iov, .msg_iovlen=1,
        .msg_control=control.bytes, .msg_controllen=sizeof(control.bytes)};
    int fd = -1;
    rhost[0] = 0;
    ssize_t n = recvmsg(channel, &msg, MSG_CMSG_CLOEXEC);
    struct cmsghdr *c = n >= 0 ? CMSG_FIRSTHDR(&msg) : NULL;
    if (c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS &&
        c->cmsg_len == CMSG_LEN(sizeof(int)))
        memcpy(&fd, CMSG_DATA(c), sizeof(fd));
    unsigned char address[16];
    size_t length = n >= 5 ? (size_t)n - 5 : 0;
    bool valid = n >= 5 && length < MAX_RHOST && !(msg.msg_flags & (MSG_CTRUNC | MSG_TRUNC)) &&
        !memcmp(data, MAGIC "\0", 5);
    if (valid) {
        memcpy(rhost, data + 5, length); rhost[length] = 0;
        valid = !length || inet_pton(AF_INET, rhost, address) == 1 ||
            inet_pton(AF_INET6, rhost, address) == 1;
    }
    if (!valid && fd >= 0) { close(fd); fd = -1; }
    if (!valid) rhost[0] = 0;
    return fd;
}
static int resume_listener(const char *path) {
    int listener = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family=AF_UNIX};
    if (strlen(path) >= sizeof(address.sun_path)) { close(listener); return -1; }
    strcpy(address.sun_path, path);
    unlink(path); // Caller holds the exclusive UID lock.
    if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof(address)) ||
        chmod(path, 0600) || listen(listener, MAX_JOBS)) {
        if (listener >= 0) close(listener);
        return -1;
    }
    return listener;
}
static int forward_resume(const char *path, int frontend, const char *rhost) {
    int channel = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family=AF_UNIX};
    if (strlen(path) >= sizeof(address.sun_path)) { close(channel); return -1; }
    strcpy(address.sun_path, path);
    char message[5 + MAX_RHOST] = {'N','R','B','2',0};
    size_t length = strnlen(rhost, MAX_RHOST - 1);
    memcpy(message + 5, rhost, length);
    int result = channel < 0 || connect(channel, (struct sockaddr *)&address, sizeof(address))
        ? -1 : send_fd(channel, message, 5 + length, frontend);
    if (channel >= 0) close(channel);
    return result;
}
static double monotonic_seconds(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) return 0;
    return (double)now.tv_sec + (double)now.tv_nsec / 1000000000.0;
}
static enum session_end serve_session_window(struct session *s, int fd, int listener,
        const char *private, uid_t uid, gid_t gid, unsigned int grace_seconds) {
    double detached_until = 0;
    enum session_end end = END_STOPPING;
    while (!stopping) {
        if (fd < 0 && monotonic_seconds() >= detached_until) { end = END_GRACE_EXPIRED; break; }
        struct pollfd p[2] = {{.fd=fd, .events=POLLIN}, {.fd=listener, .events=POLLIN}};
        int result = poll(p, 2, 200);
        if (result < 0 && errno != EINTR) { end = END_ERROR; break; }
        if (waitpid(s->worker, NULL, WNOHANG) == s->worker) { end = END_DESKTOP_EXIT; break; }
        // EOF starts the reconnect grace period, keeping PAM and applications alive.
        if (p[0].revents) {
            close(fd); fd = -1;
            detached_until = monotonic_seconds() + (double)grace_seconds;
            audit("detach", "disconnected", s->user, s->rhost, s->id, LOG_INFO);
        }
        // Check expiry before accepting a handoff arriving at the deadline.
        if (fd < 0 && monotonic_seconds() >= detached_until) { end = END_GRACE_EXPIRED; break; }
        if (p[1].revents & POLLIN) {
            int channel = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
            if (channel < 0) continue;
            struct timeval timeout = {.tv_sec=1};
            setsockopt(channel, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
            char rhost[MAX_RHOST];
            int next = receive_handoff(channel, rhost);
            close(channel);
            if (next < 0) continue;
            if (fd < 0 && monotonic_seconds() >= detached_until) {
                // This desktop is ending; a retry starts a fresh one.
                respond(next, STATUS_UNAVAILABLE, -1); close(next);
                audit("resume", "expired", s->user, rhost, s->id, LOG_NOTICE);
                end = END_GRACE_EXPIRED; break;
            }
            // An attached desktop cannot be stolen by another connection.
            if (fd >= 0) {
                respond(next, STATUS_ATTACHED, -1); close(next);
                audit("resume", status_result(STATUS_ATTACHED), s->user, rhost, s->id, LOG_NOTICE);
                continue;
            }
            int desktop = desktop_fd(private, uid, gid);
            if (desktop < 0 || respond_session(next, desktop, private, uid, gid)) {
                if (desktop >= 0) close(desktop);
                respond(next, STATUS_UNAVAILABLE, -1); close(next);
                audit("resume", status_result(STATUS_UNAVAILABLE), s->user, rhost, s->id, LOG_WARNING);
                continue;
            }
            close(desktop); fd = next;
            memcpy(s->rhost, rhost, sizeof(s->rhost));
            audit("resume", "success", s->user, s->rhost, s->id, LOG_INFO);
        }
    }
    if (fd >= 0) close(fd);
    return end;
}
static enum session_end serve_session(struct session *s, int fd, int listener,
                                      const char *private, uid_t uid, gid_t gid) {
    return serve_session_window(s, fd, listener, private, uid, gid, settings.grace_seconds);
}
static void handle_connection(int fd, int ready) {
    struct request r; struct session s = {0};
    int lock = -1, desktop = -1, resume = -1; unsigned char failure = STATUS_INVALID;
    // Outcomes after PAM and group authorization are audited at "out".
    bool authorized = false; const char *event = "login", *logout = NULL;
    char resumepath[128] = {0};
    char private[128] = {0}; uid_t uid = 0; gid_t gid = 0;
    struct timeval timeout = {.tv_sec = 10};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    // Deadlines also bound a stalled or misconfigured PAM module.
    alarm(30);
    if (read_request(fd, &r)) goto out;
    failure = STATUS_REJECTED;
    memcpy(s.rhost, r.rhost, sizeof(s.rhost));
    struct pam_conv conv = {.conv = conversation, .appdata_ptr = &r};
    if (pam_start("niri-rdp", r.user, &conv, &s.pam) != PAM_SUCCESS) {
        audit("error", "pam-start", r.user, r.rhost, NULL, LOG_ERR); goto out;
    }
    // Without a known client address PAM_RHOST stays unset.
    if (*r.rhost && pam_set_item(s.pam, PAM_RHOST, r.rhost) != PAM_SUCCESS) {
        audit("error", "pam-rhost", r.user, r.rhost, NULL, LOG_ERR); goto out;
    }
    if (pam_authenticate(s.pam, PAM_DISALLOW_NULL_AUTHTOK) != PAM_SUCCESS) {
        audit("auth-fail", "denied", r.user, r.rhost, NULL, LOG_NOTICE); goto out;
    }
    if (pam_acct_mgmt(s.pam, 0) != PAM_SUCCESS) {
        audit("auth-fail", "account-denied", r.user, r.rhost, NULL, LOG_NOTICE); goto out;
    }
    const void *name = NULL;
    if (pam_get_item(s.pam, PAM_USER, &name) != PAM_SUCCESS || !name) {
        audit("error", "pam-user", r.user, r.rhost, NULL, LOG_ERR); goto out;
    }
    struct passwd *pw = getpwnam(name);
    if (!account_allowed(pw)) {
        audit("auth-fail", "not-authorized", r.user, r.rhost, NULL, LOG_NOTICE); goto out;
    }
    // Only now may a status other than STATUS_REJECTED reach the client.
    authorized = true; failure = STATUS_UNAVAILABLE;
    size_t user_length = strnlen(pw->pw_name, MAX_USER);
    memcpy(s.user, pw->pw_name, user_length); s.user[user_length] = 0;
    uid = pw->pw_uid; gid = pw->pw_gid;
    char lockpath[128]; snprintf(lockpath, sizeof(lockpath), ROOT "/user-%lu.lock", (unsigned long)uid);
    lock = open(lockpath, O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (lock < 0) goto out;
    snprintf(resumepath, sizeof(resumepath), ROOT "/sessions/user-%lu.sock", (unsigned long)uid);
    if (flock(lock, LOCK_EX | LOCK_NB)) {
        event = "resume";
        // A holder still starting or already ending has no listener yet/anymore.
        if ((errno != EWOULDBLOCK && errno != EAGAIN) || forward_resume(resumepath, fd, r.rhost)) goto out;
        // The holder answers this connection and audits the outcome.
        failure = STATUS_NONE;
        if (write(ready, "R", 1) != 1) goto out;
        goto out;
    }
    // Lingering "closing" sessions count too: their applications may still run.
    int local = graphical_sessions(uid);
    if (local != 0) { failure = local > 0 ? STATUS_LOCAL_SESSION : STATUS_UNAVAILABLE; goto out; }
    resume = resume_listener(resumepath);
    if (resume < 0) goto out;
    if (pam_putenv(s.pam, "XDG_SESSION_TYPE=wayland") != PAM_SUCCESS ||
        pam_putenv(s.pam, "XDG_SESSION_CLASS=user") != PAM_SUCCESS ||
        pam_putenv(s.pam, "XDG_SESSION_DESKTOP=niri") != PAM_SUCCESS ||
        pam_putenv(s.pam, "XDG_SEAT=") != PAM_SUCCESS ||
        pam_putenv(s.pam, "XDG_VTNR=") != PAM_SUCCESS ||
        pam_putenv(s.pam, "DISPLAY=") != PAM_SUCCESS ||
        pam_setcred(s.pam, PAM_ESTABLISH_CRED) != PAM_SUCCESS) goto out;
    s.credentials = true;
    if (open_pam_session(&s) != PAM_SUCCESS) goto out;
    s.opened = true;
    const char *id = pam_getenv(s.pam, "XDG_SESSION_ID");
    // Record ownership before later validation/launch can fail or be killed.
    if (!valid_id(id) || write_registry(id, getpid())) goto out;
    memcpy(s.id, id, strlen(id) + 1);
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
    if (desktop < 0 || respond_session(fd, desktop, private, uid, gid)) goto out;
    failure = STATUS_NONE; // A success response must never be followed by another header.
    audit("login", "success", s.user, s.rhost, s.id, LOG_INFO);
    logout = end_result(END_ERROR);
    if (write(ready, "R", 1) != 1) goto out;
    close(ready); ready = -1;
    close(desktop); desktop = -1; alarm(0);
    enum session_end end = serve_session(&s, fd, resume, private, uid, gid);
    fd = -1;
    if (end == END_GRACE_EXPIRED)
        audit("grace-expire", "expired", s.user, s.rhost, s.id, LOG_INFO);
    logout = end_result(end);

out:
    alarm(0); wipe_request(&r);
    if (failure != STATUS_NONE) {
        respond(fd, failure, -1);
        if (authorized) audit(event, status_result(failure), s.user, s.rhost, s.id, LOG_WARNING);
    }
    if (resume >= 0) { close(resume); unlink(resumepath); }
    if (desktop >= 0) close(desktop);
    cleanup(&s);
    if (logout) audit("logout", logout, s.user, s.rhost, s.id, LOG_INFO);
    if (*private) remove_desktop(private, uid, gid);
    if (lock >= 0) close(lock);
    if (ready >= 0) close(ready);
    if (fd >= 0) close(fd);
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
    const char *why = NULL;
    if (load_config(CONFIG, 0, &settings, &why) < 0)
        sd_journal_print(LOG_WARNING, "%s rejected (%s); using defaults for every setting",
                         CONFIG, why);
    sd_journal_print(LOG_INFO, "detach grace period: %u seconds", settings.grace_seconds);
    clearenv(); setenv("PATH", "/usr/bin:/bin", 1); setenv("LANG", "C.UTF-8", 1);
    struct passwd *frontend = getpwnam("niri-rdp"); if (!frontend || frontend->pw_uid == 0) return 1;
    uid_t frontend_uid = frontend->pw_uid; gid_t frontend_gid = frontend->pw_gid;
    if ((mkdir(ROOT "/sessions", 0700) && errno != EEXIST) ||
        (mkdir(ROOT "/desktops", 0755) && errno != EEXIST)) return 1;
    // Bubblewrap opens every runtime ancestor for reading to mount D-Bus
    // proxies. Keep the parent readable; individual desktops remain 0700.
    // umask(077) would otherwise remove the parent's read/traversal permissions.
    if (chmod(ROOT "/desktops", 0755)) return 1;
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
            respond(fd, STATUS_INVALID, -1); close(fd); continue;
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
