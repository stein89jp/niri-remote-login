#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <grp.h>
#include <pwd.h>
#include <security/pam_appl.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
static int fake_setgroups(size_t count, const gid_t *groups) {
    assert(count == 0 && groups == NULL); return 0;
}
static int fake_setgid(gid_t gid) { assert(gid == getgid()); return 0; }
static int fake_setuid(uid_t uid) { assert(uid == getuid()); return 0; }
static int close_count, delete_count, end_count, order;
static int peer_skipped;
static int fail_step;
static int group_count = 1, group_calls, group_member_index;
static int group_lookup_missing, group_lookup_error;
static struct group *fake_group(const char *name) {
    assert(!strcmp(name, "niri-rdp-users"));
    static struct group group = {.gr_gid = 4242};
    return group_lookup_missing ? NULL : &group;
}
static int fake_groups(const char *user, gid_t primary, gid_t *groups, int *count) {
    assert(!strcmp(user, "alice") && primary == 1000);
    group_calls++;
    if (group_lookup_error) return -1;
    int capacity = *count;
    *count = group_count;
    for (int i = 0; i < capacity && i < group_count; i++)
        groups[i] = i == group_member_index ? 4242U : 1000U;
    return capacity < group_count ? -1 : group_count;
}
static struct pam_conv saved_conversation;
static int fake_start(const char *service, const char *user, const struct pam_conv *conv, pam_handle_t **pam) {
    assert(!strcmp(service, "niri-rdp") && !strcmp(user, "alice"));
    saved_conversation = *conv;
    if (fail_step == 0) { *pam = NULL; return PAM_SYSTEM_ERR; }
    *pam = (pam_handle_t *)1; return PAM_SUCCESS;
}
static const char *expected_rhost = "192.0.2.7";
static int rhost_items;
static int fake_item(pam_handle_t *pam, int item, const void *value) {
    // The real client address reaches PAM, never a fixed placeholder.
    assert(pam && item == PAM_RHOST && !strcmp(value, expected_rhost));
    rhost_items++; return PAM_SUCCESS;
}
static int fake_auth(pam_handle_t *pam, int flags) {
    assert(pam && flags == PAM_DISALLOW_NULL_AUTHTOK);
    struct pam_message m = {.msg_style=PAM_PROMPT_ECHO_OFF};
    const struct pam_message *message = &m; struct pam_response *response = NULL;
    assert(saved_conversation.conv(1, &message, &response, saved_conversation.appdata_ptr) == PAM_SUCCESS);
    assert(!strcmp(response->resp, "test"));
    explicit_bzero(response->resp, strlen(response->resp)); free(response->resp); free(response);
    return fail_step == 1 ? PAM_AUTH_ERR : PAM_SUCCESS;
}
static int fake_account(pam_handle_t *pam, int flags) {
    assert(pam && flags == 0); return fail_step == 2 ? PAM_ACCT_EXPIRED : PAM_SUCCESS;
}
static const char *pam_user_name = "root";
static int fake_get_item(const pam_handle_t *pam, int item, const void **value) {
    assert(pam && item == PAM_USER); *value = pam_user_name; return PAM_SUCCESS;
}
static struct passwd *fake_getpwnam(const char *name) {
    static struct passwd alice = {.pw_name="alice", .pw_uid=1000, .pw_gid=1000,
        .pw_dir="/home/alice", .pw_shell="/bin/sh"};
    static struct passwd root = {.pw_name="root", .pw_uid=0, .pw_gid=0,
        .pw_dir="/root", .pw_shell="/bin/sh"};
    return !strcmp(name, "alice") ? &alice : !strcmp(name, "root") ? &root : NULL;
}
static int putenv_result = PAM_SYSTEM_ERR;
static int fake_putenv(pam_handle_t *pam, const char *value) {
    assert(pam && value); return putenv_result;
}
static mode_t pam_session_umask; // 0: pam_umask is not in the stack.
static int fake_open(pam_handle_t *pam, int flags) {
    assert(pam && flags == 0);
    if (pam_session_umask) umask(pam_session_umask);
    return PAM_SUCCESS;
}
static int local_sessions; // >0 wayland sessions, <0 logind error.
static int fake_uid_sessions(uid_t uid, int require_active, char ***sessions) {
    assert(uid == 1000 && require_active == -1);
    if (local_sessions < 0) return -EIO;
    *sessions = calloc((size_t)local_sessions + 1, sizeof(char *));
    for (int i = 0; i < local_sessions; i++) (*sessions)[i] = strdup("c9");
    return local_sessions;
}
static int fake_session_type(const char *session, char **type) {
    assert(!strcmp(session, "c9")); *type = strdup("wayland"); return 0;
}
// Audit entries as "event result user rhost session"; a forked holder also
// copies them to audit_fd so the parent can inspect them.
static char audit_log[16384];
static int audit_fd = -1;
static void test_audit(const char *event, const char *result, const char *user,
                       const char *rhost, const char *id, int priority) {
    assert(priority >= 0 && priority <= 7);
    char line[512];
    int n = snprintf(line, sizeof(line), "%s %s %s %s %s\n", event, result, user, rhost, id);
    assert(n > 0 && (size_t)n < sizeof(line));
    size_t used = strlen(audit_log);
    assert(used + (size_t)n < sizeof(audit_log));
    memcpy(audit_log + used, line, (size_t)n + 1);
    if (audit_fd >= 0) assert(write(audit_fd, line, (size_t)n) == n);
}
static void reset_audit(void) { audit_log[0] = 0; }
static bool audited(const char *line) { return strstr(audit_log, line) != NULL; }
static int fake_close(pam_handle_t *pam, int flags) {
    assert(pam); assert(flags == 0); assert(order++ == 0); close_count++; return PAM_SUCCESS;
}
static int fake_cred(pam_handle_t *pam, int flags) {
    assert(pam); assert(flags == PAM_DELETE_CRED); assert(order++ == (close_count ? 1 : 0));
    delete_count++; return PAM_SUCCESS;
}
static int fake_end(pam_handle_t *pam, int status) {
    assert(pam); assert(status == PAM_SUCCESS); end_count++; return PAM_SUCCESS;
}
#define pam_close_session fake_close
#define pam_setcred fake_cred
#define pam_end fake_end
#define pam_start fake_start
#define pam_set_item fake_item
#define pam_authenticate fake_auth
#define pam_acct_mgmt fake_account
#define pam_get_item fake_get_item
#define pam_putenv fake_putenv
#define pam_open_session fake_open
#define getpwnam fake_getpwnam
#define sd_uid_get_sessions fake_uid_sessions
#define sd_session_get_type fake_session_type
#define AUDIT_SINK test_audit
#define ROOT "broker-test-root"
#define getgrnam fake_group
#define getgrouplist fake_groups
#define setgroups fake_setgroups
#define setgid fake_setgid
#define setuid fake_setuid
#define WORKER "/nonexistent/niri-rdp-worker-test"
#define BROKER_TEST
#include "broker.c"

static void reset_counts(void) { close_count = delete_count = end_count = order = 0; }
static void test_request_bounds(void) {
    int p[2]; assert(socketpair(AF_UNIX, SOCK_STREAM, 0, p) == 0);
    unsigned char good[] = {'N','R','B','2',0,5,0,4,4,192,0,2,7,'a','l','i','c','e','t','e','s','t'};
    assert(write(p[0], good, sizeof(good)) == (ssize_t)sizeof(good));
    struct request r; assert(read_request(p[1], &r) == 0);
    assert(!strcmp(r.user, "alice") && !strcmp(r.password, "test") && !strcmp(r.rhost, "192.0.2.7"));
    wipe_request(&r); for (size_t i = 0; i < sizeof(r); i++) assert(((char *)&r)[i] == 0);
    unsigned char v6[] = {'N','R','B','2',0,1,0,1,6,0x20,0x01,0x0d,0xb8,0,0,0,0,0,0,0,0,0,0,0,1,'a','x'};
    assert(write(p[0], v6, sizeof(v6)) == (ssize_t)sizeof(v6));
    assert(read_request(p[1], &r) == 0 && !strcmp(r.rhost, "2001:db8::1") && !strcmp(r.user, "a"));
    unsigned char unknown[] = {'N','R','B','2',0,1,0,1,0,'a','x'};
    assert(write(p[0], unknown, sizeof(unknown)) == (ssize_t)sizeof(unknown));
    assert(read_request(p[1], &r) == 0 && r.rhost[0] == 0 && !strcmp(r.password, "x"));
    unsigned char large[] = {'N','R','B','2',0,65,0,1,0};
    assert(write(p[0], large, sizeof(large)) == (ssize_t)sizeof(large));
    assert(read_request(p[1], &r) == -1);
    close(p[0]); close(p[1]);
    for (unsigned int scenario = 0; scenario < 8; scenario++) {
        assert(socketpair(AF_UNIX, SOCK_STREAM, 0, p) == 0);
        unsigned char bad[] = {'N','R','B','2',0,1,0,1,0,'a','x'};
        if (scenario == 0) bad[0] = 'X';
        if (scenario == 1) bad[5] = 0;
        if (scenario == 2) bad[9] = 0;
        if (scenario == 3) bad[10] = 0;
        if (scenario == 4) bad[9] = '/';
        // A space could forge "rhost=" fields in audit entries.
        if (scenario == 5) bad[9] = ' ';
        if (scenario == 6) bad[8] = 5;
        // The previous protocol version is not accepted.
        if (scenario == 7) bad[3] = '1';
        assert(write(p[0], bad, sizeof(bad)) == (ssize_t)sizeof(bad)); shutdown(p[0], SHUT_WR);
        assert(read_request(p[1], &r) == -1); close(p[0]); close(p[1]);
    }
    puts("PASS bounded v2 protocol with client address, malformed identity, zeroized credentials");
}
static void test_conversation(void) {
    struct request r = {.user="alice", .password="secret"};
    struct pam_message pass = {.msg_style=PAM_PROMPT_ECHO_OFF, .msg="Password"};
    struct pam_message user = {.msg_style=PAM_PROMPT_ECHO_ON, .msg="User"};
    const struct pam_message *messages[] = {&user, &pass};
    struct pam_response *out = NULL;
    assert(conversation(2, messages, &out, &r) == PAM_SUCCESS);
    assert(!strcmp(out[0].resp,"alice") && !strcmp(out[1].resp,"secret"));
    explicit_bzero(out[1].resp, strlen(out[1].resp)); free(out[0].resp); free(out[1].resp); free(out);
    const struct pam_message *repeated[] = {&pass, &pass};
    assert(conversation(2, repeated, &out, &r) == PAM_CONV_ERR);
    assert(conversation(9, messages, &out, &r) == PAM_CONV_ERR);
    assert(conversation(0, messages, &out, &r) == PAM_CONV_ERR);
    puts("PASS password/username conversation and unsupported multi-prompt rejection");
}
static void test_rollback(void) {
    for (unsigned int state = 0; state < 4; state++) {
        reset_counts();
        struct session s = {.pam=state ? (pam_handle_t *)1 : NULL,
            .credentials=state >= 2, .opened=state >= 3};
        cleanup(&s);
        assert(close_count == (state >= 3)); assert(delete_count == (state >= 2));
        assert(end_count == (state >= 1)); assert(s.pam == NULL && !s.credentials && !s.opened);
        cleanup(&s); assert(end_count == (state >= 1));
    }
    puts("PASS acquired-resource rollback order and idempotent cleanup");
}
static void test_fd_transfer(void) {
    int pair[2], worker[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, worker) == 0);
    assert(respond(pair[0], 0, worker[1]) == 0);
    unsigned char bytes[5]; struct iovec iov = {.iov_base=bytes, .iov_len=sizeof(bytes)};
    union { struct cmsghdr align; char data[CMSG_SPACE(sizeof(int))]; } ancillary;
    struct msghdr msg = {.msg_iov=&iov, .msg_iovlen=1,
        .msg_control=ancillary.data, .msg_controllen=sizeof(ancillary.data)};
    assert(recvmsg(pair[1], &msg, MSG_CMSG_CLOEXEC) == 5);
    assert(!memcmp(bytes, "NRB2\0", 5));
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg); assert(c && c->cmsg_type == SCM_RIGHTS);
    int received; memcpy(&received, CMSG_DATA(c), sizeof(received));
    assert(fcntl(received, F_GETFD) & FD_CLOEXEC);
    assert(write(worker[0], "W", 1) == 1); char b; assert(read(received, &b, 1) == 1 && b == 'W');
    close(received); close(worker[0]); close(worker[1]); close(pair[0]); close(pair[1]);
    puts("PASS authenticated-session Wayland descriptor handoff");
}
static void test_process_cleanup(void) {
    int ready[2]; assert(pipe(ready) == 0);
    pid_t worker = fork(); assert(worker >= 0);
    if (!worker) {
        close(ready[0]); assert(setsid() >= 0);
        pid_t descendant = fork(); assert(descendant >= 0);
        if (!descendant) { close(ready[1]); for (;;) pause(); }
        assert(write(ready[1], &descendant, sizeof(descendant)) == (ssize_t)sizeof(descendant));
        close(ready[1]); _exit(0);
    }
    close(ready[1]); pid_t descendant;
    assert(read_full(ready[0], &descendant, sizeof(descendant)) == 0); close(ready[0]);
    assert(waitpid(worker, NULL, 0) == worker);
    struct session s = {.worker=worker}; cleanup(&s); assert(s.worker == 0);
    char path[64]; snprintf(path, sizeof(path), "/proc/%ld/stat", (long)descendant);
    bool dead = false;
    for (int i=0;i<100;i++) {
        FILE *f=fopen(path,"r"); if (!f) { dead=true; break; }
        char line[1024]; assert(fgets(line,sizeof(line),f)); fclose(f);
        char *state=strrchr(line,')'); if (state && state[2]=='Z') { dead=true; break; }
        usleep(10000);
    }
    assert(dead); puts("PASS descendants killed after compositor group leader exits");
}
static const unsigned char alice_request[] =
    {'N','R','B','2',0,5,0,4,4,192,0,2,7,'a','l','i','c','e','t','e','s','t'};
// Runs one broker connection; returns the status byte, or -1 without a reply.
static int broker_status(const unsigned char *request, size_t size, char *ready) {
    int pair[2], notification[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0); assert(pipe(notification) == 0);
    assert(write(pair[0], request, size) == (ssize_t)size);
    handle_connection(pair[1], notification[1]);
    shutdown(pair[0], SHUT_WR);
    unsigned char response[5]; int status = -1;
    if (read_full(pair[0], response, 5) == 0) {
        assert(!memcmp(response, "NRB2", 4)); status = response[4];
    }
    char byte; *ready = read(notification[0], &byte, 1) == 1 ? byte : 0;
    close(pair[0]); close(notification[0]);
    return status;
}
static void test_auth_failures(void) {
    const char *expected[] = {"error pam-start alice 192.0.2.7 -", "auth-fail denied alice 192.0.2.7 -",
        "auth-fail account-denied alice 192.0.2.7 -", "auth-fail not-authorized alice 192.0.2.7 -"};
    for (fail_step = 0; fail_step < 4; fail_step++) {
        reset_counts(); reset_audit(); rhost_items = 0; char ready;
        assert(broker_status(alice_request, sizeof(alice_request), &ready) == STATUS_REJECTED);
        assert(!close_count && !delete_count && end_count == (fail_step > 0) && !ready);
        assert(rhost_items == (fail_step > 0));
        // Exactly one entry, and it never contains the password.
        assert(audited(expected[fail_step]) && strchr(audit_log, '\n') == audit_log + strlen(audit_log) - 1);
        assert(!strstr(audit_log, "test"));
    }
    // Without a client address PAM_RHOST stays unset.
    unsigned char anonymous[] = {'N','R','B','2',0,5,0,4,0,'a','l','i','c','e','t','e','s','t'};
    reset_counts(); reset_audit(); rhost_items = 0; char ready;
    assert(broker_status(anonymous, sizeof(anonymous), &ready) == STATUS_REJECTED);
    assert(rhost_items == 0 && audited("auth-fail not-authorized alice - -"));
    puts("PASS PAM init/auth/account failure and final PAM identity authorization reject without sessions");
}
static void test_status_codes(void) {
    // Distinct codes appear only after PAM and group authorization succeed.
    assert(mkdir(ROOT, 0700) == 0 && mkdir(ROOT "/sessions", 0700) == 0);
    pam_user_name = "alice"; fail_step = 4; group_count = 1; group_member_index = 0;
    char ready;
    reset_counts(); reset_audit(); local_sessions = 1;
    assert(broker_status(alice_request, sizeof(alice_request), &ready) == STATUS_LOCAL_SESSION);
    assert(audited("login local-session alice 192.0.2.7 -") && !ready);
    assert(!close_count && !delete_count && end_count == 1);
    reset_audit(); local_sessions = -1;
    assert(broker_status(alice_request, sizeof(alice_request), &ready) == STATUS_UNAVAILABLE);
    assert(audited("login unavailable alice 192.0.2.7 -"));
    // Startup failure after the local-session check is still "try again".
    reset_audit(); local_sessions = 0;
    assert(broker_status(alice_request, sizeof(alice_request), &ready) == STATUS_UNAVAILABLE);
    assert(audited("login unavailable alice 192.0.2.7 -"));
    assert(access(ROOT "/sessions/user-1000.sock", F_OK) == -1);
    // A non-member with the right password still gets the generic rejection.
    reset_audit(); group_member_index = -1;
    assert(broker_status(alice_request, sizeof(alice_request), &ready) == STATUS_REJECTED);
    assert(audited("auth-fail not-authorized"));
    group_member_index = 0;
    // A running holder: the second login is forwarded with its client address.
    int holder = open(ROOT "/user-1000.lock", O_RDWR | O_CLOEXEC);
    assert(holder >= 0 && flock(holder, LOCK_EX | LOCK_NB) == 0);
    reset_audit();
    assert(broker_status(alice_request, sizeof(alice_request), &ready) == STATUS_UNAVAILABLE);
    assert(audited("resume unavailable alice 192.0.2.7 -"));
    int listener = resume_listener(ROOT "/sessions/user-1000.sock"); assert(listener >= 0);
    reset_audit();
    int pair[2], notification[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0 && pipe(notification) == 0);
    assert(write(pair[0], alice_request, sizeof(alice_request)) == (ssize_t)sizeof(alice_request));
    handle_connection(pair[1], notification[1]);
    char byte; assert(read(notification[0], &byte, 1) == 1 && byte == 'R'); close(notification[0]);
    int channel = accept4(listener, NULL, NULL, SOCK_CLOEXEC); assert(channel >= 0);
    char rhost[MAX_RHOST]; int forwarded = receive_handoff(channel, rhost); close(channel);
    assert(forwarded >= 0 && !strcmp(rhost, "192.0.2.7"));
    // The holder, not the forwarding child, answers and audits.
    assert(audit_log[0] == 0);
    assert(respond(forwarded, STATUS_ATTACHED, -1) == 0); close(forwarded);
    unsigned char response[5]; assert(read_full(pair[0], response, 5) == 0);
    assert(!memcmp(response, "NRB2", 4) && response[4] == STATUS_ATTACHED);
    assert(read(pair[0], response, 1) == 0); close(pair[0]);
    close(listener); unlink(ROOT "/sessions/user-1000.sock"); close(holder);
    assert(unlink(ROOT "/user-1000.lock") == 0);
    assert(rmdir(ROOT "/sessions") == 0 && rmdir(ROOT) == 0);
    pam_user_name = "root"; fail_step = 0; local_sessions = 0;
    puts("PASS post-authorization status codes, generic non-member rejection, RHOST handoff to holder");
}
static void test_launch_failure(void) {
    reset_counts();
    struct session s = {.pam=(pam_handle_t *)1, .credentials=true, .opened=true};
    struct passwd pw = {.pw_uid=getuid(), .pw_gid=getgid()};
    assert(launch(&s, &pw, "/unused", "c1", "/unused", -1, -1) == -1);
    assert(s.worker == 0); cleanup(&s);
    assert(close_count == 1 && delete_count == 1 && end_count == 1);
    puts("PASS fixed executable launch rejection closes opened PAM session and credentials");
}
static void test_group_authorization(void) {
    struct passwd pw = {.pw_name="alice", .pw_uid=1000, .pw_gid=1000,
        .pw_dir="/home/alice", .pw_shell="/bin/sh"};
    group_count = 1; group_member_index = 0; group_calls = 0;
    assert(account_allowed(&pw) && group_calls == 1);
    group_count = 65; group_member_index = 64; group_calls = 0;
    assert(account_allowed(&pw) && group_calls == 2);
    group_member_index = -1; group_calls = 0;
    assert(!account_allowed(&pw) && group_calls == 2);
    group_lookup_error = 1; group_calls = 0;
    assert(!account_allowed(&pw) && group_calls == 1);
    group_lookup_error = 0; group_lookup_missing = 1; group_calls = 0;
    assert(!account_allowed(&pw) && group_calls == 0);
    group_lookup_missing = 0;
    puts("PASS group authorization resizes supplementary-group buffer and fails closed on lookup errors");
}
static void test_peer(void) {
    int pair[2]; assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    struct ucred credentials; socklen_t size = sizeof(credentials);
    if (getsockopt(pair[1], SOL_SOCKET, SO_PEERCRED, &credentials, &size) < 0 && errno == EPERM) {
        assert(!peer_allowed(pair[1], getuid()));
        close(pair[0]); close(pair[1]);
        puts("SKIP positive SO_PEERCRED: execution sandbox denies syscall; fail-closed rejection passed");
        peer_skipped = 1;
        return;
    }
    assert(peer_allowed(pair[1], getuid())); assert(!peer_allowed(pair[1], getuid() + 1));
    close(pair[0]); close(pair[1]);
    puts("PASS SO_PEERCRED requires exact authorized frontend UID");
}
static int resume_response(int fd, unsigned char expected) {
    struct pollfd p = {.fd=fd, .events=POLLIN};
    assert(poll(&p, 1, 3000) == 1);
    unsigned char bytes[5]; struct iovec iov = {.iov_base=bytes, .iov_len=sizeof(bytes)};
    union { struct cmsghdr align; char bytes[CMSG_SPACE(2 * sizeof(int))]; } ancillary;
    struct msghdr msg = {.msg_iov=&iov, .msg_iovlen=1,
        .msg_control=ancillary.bytes, .msg_controllen=sizeof(ancillary.bytes)};
    assert(recvmsg(fd, &msg, MSG_CMSG_CLOEXEC) == 5);
    assert(!memcmp(bytes, "NRB2", 4) && bytes[4] == expected);
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    if (expected) { assert(c == NULL); return -1; }
    assert(c && c->cmsg_type == SCM_RIGHTS);
    assert(c->cmsg_len == CMSG_LEN(2 * sizeof(int)));
    int descriptors[2]; memcpy(descriptors, CMSG_DATA(c), sizeof(descriptors));
    int desktop = descriptors[0]; close(descriptors[1]);
    return desktop;
}
static void test_resume_session(void) {
    char directory[] = "/tmp/niri-rdp-resume-XXXXXX";
    assert(mkdtemp(directory));
    char path[128]; assert(snprintf(path, sizeof(path), "%s/resume", directory) > 0);
    int listener = resume_listener(path); assert(listener >= 0);
    struct stat st; assert(stat(path, &st) == 0 && (st.st_mode & 0777) == 0600);
    struct sockaddr_un address = {.sun_family=AF_UNIX};
    assert(snprintf(address.sun_path, sizeof(address.sun_path), "%s/wayland-0", directory) > 0);
    int wayland = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0); assert(wayland >= 0);
    assert(bind(wayland, (struct sockaddr *)&address, sizeof(address)) == 0);
    assert(listen(wayland, 4) == 0);
    int audio = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0); assert(audio >= 0);
    assert(snprintf(address.sun_path, sizeof(address.sun_path), "%s/audio-0", directory) > 0);
    assert(bind(audio, (struct sockaddr *)&address, sizeof(address)) == 0);
    assert(listen(audio, 4) == 0);
    int initial[2], notification[2], worker_stop[2], log[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, initial) == 0);
    assert(pipe(notification) == 0 && pipe(worker_stop) == 0 && pipe(log) == 0);
    pid_t holder = fork(); assert(holder >= 0);
    if (!holder) {
        alarm(10); close(initial[0]); close(notification[0]); close(worker_stop[1]); close(wayland);
        close(log[0]); audit_fd = log[1];
        pid_t worker = fork(); assert(worker >= 0);
        if (!worker) {
            close(initial[1]); close(notification[1]); close(listener);
            char byte; assert(read(worker_stop[0], &byte, 1) == 1); _exit(0);
        }
        close(worker_stop[0]);
        assert(write(notification[1], &worker, sizeof(worker)) == (ssize_t)sizeof(worker));
        close(notification[1]);
        struct session s = {.worker=worker, .user="alice", .rhost="198.51.100.1", .id="c7"};
        assert(serve_session_window(&s, initial[1], listener, directory, getuid(), getgid(), 1) ==
               END_DESKTOP_EXIT);
        close(listener); _exit(0);
    }
    close(initial[1]); close(notification[1]); close(worker_stop[0]); close(listener); close(log[1]);
    pid_t worker; assert(read_full(notification[0], &worker, sizeof(worker)) == 0);
    close(notification[0]);
    int retry[2]; assert(socketpair(AF_UNIX, SOCK_STREAM, 0, retry) == 0);
    assert(forward_resume(path, retry[1], "192.0.2.9") == 0); close(retry[1]);
    assert(resume_response(retry[0], STATUS_ATTACHED) == -1); close(retry[0]);
    close(initial[0]);
    // Multiple detach/resume cycles must all use the same surviving worker and socket.
    for (int cycle = 0; cycle < 3; cycle++) {
        assert(socketpair(AF_UNIX, SOCK_STREAM, 0, retry) == 0);
        assert(forward_resume(path, retry[1], cycle == 1 ? "2001:db8::5" : "192.0.2.10") == 0);
        close(retry[1]);
        int desktop = resume_response(retry[0], 0);
        int audio_peer = accept4(audio, NULL, NULL, SOCK_CLOEXEC); assert(audio_peer >= 0); close(audio_peer);
        struct pollfd p = {.fd=wayland, .events=POLLIN}; assert(poll(&p, 1, 3000) == 1);
        int peer = accept4(wayland, NULL, NULL, SOCK_CLOEXEC); assert(peer >= 0);
        assert(kill(worker, 0) == 0 && waitpid(holder, NULL, WNOHANG) == 0);
        assert(write(peer, "W", 1) == 1);
        char byte; assert(read(desktop, &byte, 1) == 1 && byte == 'W');
        // Total elapsed time exceeds one grace period; attaching resets each deadline.
        usleep(600000);
        close(peer); close(desktop); close(retry[0]);
        usleep(200000);
    }
    assert(write(worker_stop[1], "X", 1) == 1); close(worker_stop[1]);
    int status; assert(waitpid(holder, &status, 0) == holder);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    reset_audit();
    ssize_t logged = read(log[0], audit_log, sizeof(audit_log) - 1); close(log[0]);
    assert(logged > 0); audit_log[logged] = 0;
    // Each resume and detach is audited with the address of that client.
    assert(audited("resume already-attached alice 192.0.2.9 c7\n"
                   "detach disconnected alice 198.51.100.1 c7\n"
                   "resume success alice 192.0.2.10 c7\n"
                   "detach disconnected alice 192.0.2.10 c7\n"
                   "resume success alice 2001:db8::5 c7\n"
                   "detach disconnected alice 2001:db8::5 c7\n"));
    close(audio); assert(unlink(address.sun_path) == 0);
    assert(snprintf(address.sun_path, sizeof(address.sun_path), "%s/wayland-0", directory) > 0);
    close(wayland); assert(unlink(path) == 0); assert(unlink(address.sun_path) == 0);
    assert(rmdir(directory) == 0);
    puts("PASS attached session rejects takeover; repeated detach/resume retains worker and desktop; worker exit ends holder");
}
static void test_detach_timeout(void) {
    int initial[2], ready[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, initial) == 0 && pipe(ready) == 0);
    pid_t holder = fork(); assert(holder >= 0);
    if (!holder) {
        alarm(6); close(initial[0]); close(ready[0]); reset_counts();
        int worker_ready[2]; assert(pipe(worker_ready) == 0);
        pid_t worker = fork(); assert(worker >= 0);
        if (!worker) {
            close(initial[1]); close(ready[1]); close(worker_ready[0]);
            assert(setsid() >= 0);
            assert(write(worker_ready[1], "R", 1) == 1); close(worker_ready[1]);
            for (;;) pause();
        }
        close(worker_ready[1]); char byte;
        assert(read(worker_ready[0], &byte, 1) == 1); close(worker_ready[0]);
        assert(write(ready[1], &worker, sizeof(worker)) == (ssize_t)sizeof(worker)); close(ready[1]);
        struct session s = {.worker=worker, .pam=(pam_handle_t *)1, .credentials=true, .opened=true};
        assert(serve_session_window(&s, initial[1], -1, "/unused", getuid(), getgid(), 1) ==
               END_GRACE_EXPIRED);
        assert(kill(worker, 0) == 0);
        cleanup(&s);
        assert(s.worker == 0 && close_count == 1 && delete_count == 1 && end_count == 1);
        assert(kill(worker, 0) == -1 && errno == ESRCH);
        _exit(0);
    }
    close(initial[1]); close(ready[1]); pid_t worker;
    assert(read_full(ready[0], &worker, sizeof(worker)) == 0); close(ready[0]);
    close(initial[0]);
    int status; assert(waitpid(holder, &status, 0) == holder);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    assert(kill(worker, 0) == -1 && errno == ESRCH);
    puts("PASS expired detach grace ends holder and cleans worker, PAM session and credentials");
}
static void test_desktop_without_audio(void) {
    int response[2], desktop[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, response) == 0);
    assert(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, desktop) == 0);
    assert(respond_session(response[1], desktop[0], "/no-private-audio-here", getuid(), getgid()) == 0);
    unsigned char bytes[5];
    struct iovec iov = {.iov_base=bytes, .iov_len=sizeof(bytes)};
    union { struct cmsghdr align; char bytes[CMSG_SPACE(2 * sizeof(int))]; } ancillary;
    struct msghdr msg = {.msg_iov=&iov, .msg_iovlen=1,
        .msg_control=ancillary.bytes, .msg_controllen=sizeof(ancillary.bytes)};
    assert(recvmsg(response[0], &msg, MSG_CMSG_CLOEXEC) == 5);
    assert(!memcmp(bytes, "NRB2\0", 5));
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    assert(c && c->cmsg_type == SCM_RIGHTS && c->cmsg_len == CMSG_LEN(sizeof(int)));
    int received; memcpy(&received, CMSG_DATA(c), sizeof(received));
    assert(write(desktop[1], "D", 1) == 1);
    char byte; assert(read(received, &byte, 1) == 1 && byte == 'D');
    close(received); close(response[0]); close(response[1]); close(desktop[0]); close(desktop[1]);
    puts("PASS unavailable ancillary audio preserves authenticated desktop descriptor handoff");
}
static int session_listener(const char *directory, const char *name) {
    struct sockaddr_un address = {.sun_family=AF_UNIX};
    assert(snprintf(address.sun_path, sizeof(address.sun_path), "%s/%s", directory, name) > 0);
    int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0); assert(listener >= 0);
    assert(bind(listener, (struct sockaddr *)&address, sizeof(address)) == 0);
    assert(listen(listener, 4) == 0);
    return listener;
}
// Each received descriptor must reach exactly its own listener, in order.
static void assert_reaches(int received, int listener, char marker) {
    int accepted = accept4(listener, NULL, NULL, SOCK_CLOEXEC); assert(accepted >= 0);
    assert(fcntl(received, F_GETFD) & FD_CLOEXEC);
    assert(write(received, &marker, 1) == 1);
    char byte; assert(read(accepted, &byte, 1) == 1 && byte == marker);
    close(accepted);
}
static void test_microphone_descriptor(void) {
    // [desktop, audio, microphone] when both helpers listen; the old two- and
    // one-descriptor replies when the microphone or playback is missing.
    for (int scenario = 0; scenario < 3; scenario++) {
        char directory[] = "/tmp/niri-rdp-microphone-XXXXXX"; assert(mkdtemp(directory));
        int audio = scenario != 2 ? session_listener(directory, "audio-0") : -1;
        int microphone = scenario != 1 ? session_listener(directory, "audio-in-0") : -1;
        size_t expected = scenario == 0 ? 3 : scenario == 1 ? 2 : 1;
        int response[2], desktop[2];
        assert(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, response) == 0);
        assert(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, desktop) == 0);
        assert(respond_session(response[1], desktop[0], directory, getuid(), getgid()) == 0);
        unsigned char bytes[5];
        struct iovec iov = {.iov_base=bytes, .iov_len=sizeof(bytes)};
        union { struct cmsghdr align; char bytes[CMSG_SPACE(4 * sizeof(int))]; } ancillary;
        struct msghdr msg = {.msg_iov=&iov, .msg_iovlen=1,
            .msg_control=ancillary.bytes, .msg_controllen=sizeof(ancillary.bytes)};
        assert(recvmsg(response[0], &msg, MSG_CMSG_CLOEXEC) == 5);
        assert(!memcmp(bytes, "NRB2\0", 5) && !(msg.msg_flags & MSG_CTRUNC));
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        assert(c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS);
        assert(c->cmsg_len == CMSG_LEN(expected * sizeof(int)) && !CMSG_NXTHDR(&msg, c));
        int received[3]; memcpy(received, CMSG_DATA(c), expected * sizeof(int));
        assert(fcntl(received[0], F_GETFD) & FD_CLOEXEC);
        assert(write(desktop[1], "D", 1) == 1);
        char byte; assert(read(received[0], &byte, 1) == 1 && byte == 'D');
        if (expected > 1) assert_reaches(received[1], audio, 'A');
        if (expected > 2) assert_reaches(received[2], microphone, 'M');
        // Nothing else connected: no stray descriptor was created for a
        // microphone helper behind a missing playback socket.
        if (scenario == 2) {
            int flags = fcntl(microphone, F_GETFL); assert(flags >= 0);
            assert(fcntl(microphone, F_SETFL, flags | O_NONBLOCK) == 0);
            assert(accept(microphone, NULL, NULL) < 0 && errno == EAGAIN);
        }
        for (size_t i = 0; i < expected; i++) close(received[i]);
        if (audio >= 0) close(audio);
        if (microphone >= 0) close(microphone);
        close(response[0]); close(response[1]); close(desktop[0]); close(desktop[1]);
        char path[128];
        assert(snprintf(path, sizeof(path), "%s/audio-0", directory) > 0); unlink(path);
        assert(snprintf(path, sizeof(path), "%s/audio-in-0", directory) > 0); unlink(path);
        assert(rmdir(directory) == 0);
    }
    puts("PASS microphone descriptor follows playback in order and never displaces it");
}
static void wait_success(pid_t child) {
    int status; assert(waitpid(child, &status, 0) == child);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
static void test_session_limits(void) {
    // Desktop processes must not inherit the broker's umask 077 or core 0/0.
    pid_t child = fork(); assert(child >= 0);
    if (!child) {
        umask(077);
        struct rlimit before; assert(getrlimit(RLIMIT_CORE, &before) == 0);
        bool root = geteuid() == 0;
        if (root) {
            struct rlimit zero = {0, 0}; assert(setrlimit(RLIMIT_CORE, &zero) == 0);
        }
        struct session s = {0};
        session_process_limits(&s);
        assert(umask(077) == 022);
        struct rlimit after; assert(getrlimit(RLIMIT_CORE, &after) == 0);
        // Without privilege the hard limit cannot be raised, only kept.
        assert(after.rlim_cur == 0 && after.rlim_max == (root ? RLIM_INFINITY : before.rlim_max));
        // A PAM session's choices (pam_umask, pam_limits) win.
        s.limits = true; s.umask = 027; s.core = (struct rlimit){0, after.rlim_max};
        session_process_limits(&s);
        assert(umask(077) == 027);
        _exit(0);
    }
    wait_success(child);
    child = fork(); assert(child >= 0);
    if (!child) {
        umask(077);
        struct rlimit before; assert(getrlimit(RLIMIT_CORE, &before) == 0);
        for (int stacked = 0; stacked < 2; stacked++) {
            pam_session_umask = stacked ? 0027 : 0;
            struct session s = {.pam=(pam_handle_t *)1};
            assert(open_pam_session(&s) == PAM_SUCCESS && s.limits);
            // Without pam_umask the usual login default applies.
            assert(s.umask == (stacked ? 0027 : 022) && s.core.rlim_cur == 0);
            if (geteuid() == 0) assert(s.core.rlim_max == RLIM_INFINITY);
            // The broker keeps its own umask and core limit.
            assert(umask(077) == 077);
            struct rlimit after; assert(getrlimit(RLIMIT_CORE, &after) == 0);
            assert(after.rlim_cur == before.rlim_cur && after.rlim_max == before.rlim_max);
        }
        _exit(0);
    }
    wait_success(child);
    puts("PASS desktop umask/core limit follow the PAM session or 022 and 0/unlimited; broker keeps its own");
}
static void config_file(const char *path, const char *text, size_t size, mode_t mode) {
    unlink(path);
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600); assert(fd >= 0);
    assert(write(fd, text, size) == (ssize_t)size); close(fd);
    assert(chmod(path, mode) == 0);
}
static int config_result(const char *path, const char *text, mode_t mode, unsigned int *grace) {
    config_file(path, text, strlen(text), mode);
    struct config c = {.grace_seconds = 9999}; const char *why = "unset";
    int result = load_config(path, getuid(), &c, &why);
    // Every outcome other than success leaves all defaults and gives a reason.
    if (result) assert(c.grace_seconds == 45 && why && *why);
    else assert(why == NULL);
    *grace = c.grace_seconds;
    return result;
}
static void test_config(void) {
    char directory[] = "/tmp/niri-rdp-config-XXXXXX"; assert(mkdtemp(directory));
    char path[128], link_path[128];
    snprintf(path, sizeof(path), "%s/broker.conf", directory);
    snprintf(link_path, sizeof(link_path), "%s/link.conf", directory);
    struct config c; const char *why;
    assert(load_config(path, getuid(), &c, &why) == 1 && c.grace_seconds == 45 && !why);
    unsigned int grace;
    assert(config_result(path, "# comment\n\n  grace_seconds = 120  \n", 0644, &grace) == 0 && grace == 120);
    assert(config_result(path, "grace_seconds=0", 0600, &grace) == 0 && grace == 0);
    assert(config_result(path, "\tgrace_seconds\t=\t3600\n# end\n", 0644, &grace) == 0 && grace == 3600);
    assert(config_result(path, "", 0644, &grace) == 0 && grace == 45);
    const char *bad[] = {"grace_seconds = 3601\n", "grace_seconds = -1\n", "grace_seconds = +5\n",
        "grace_seconds = 12s\n", "grace_seconds = \n", "grace_seconds 45\n", "= 45\n",
        "grace_seconds = 45\ngrace_seconds = 46\n", "max_sessions_per_user = 1\n",
        "Grace_seconds = 4\n", "grace_seconds = 45 # trailing\n", "grace_seconds = 45\r\n",
        "grace_seconds = 99999999999999999999\n", "grace_seconds = 0x10\n", "[broker]\n"};
    for (size_t i = 0; i < sizeof(bad) / sizeof(*bad); i++)
        assert(config_result(path, bad[i], 0644, &grace) == -1);
    // Modes, ownership, links, file types, size and NUL bytes are strict too.
    assert(config_result(path, "grace_seconds = 10\n", 0664, &grace) == -1);
    assert(config_result(path, "grace_seconds = 10\n", 0646, &grace) == -1);
    assert(load_config(path, getuid() + 1, &c, &why) == -1 && c.grace_seconds == 45 &&
           !strcmp(why, "wrong owner"));
    assert(chmod(path, 0644) == 0 && symlink(path, link_path) == 0);
    assert(load_config(path, getuid(), &c, &why) == 0 && c.grace_seconds == 10);
    assert(load_config(link_path, getuid(), &c, &why) == -1 && c.grace_seconds == 45);
    assert(load_config(directory, getuid(), &c, &why) == -1);
    char large[MAX_CONFIG + 64]; memset(large, '#', sizeof(large)); large[sizeof(large) - 1] = 0;
    assert(config_result(path, large, 0644, &grace) == -1);
    config_file(path, "grace_seconds = 10\n\0", 20, 0644);
    assert(load_config(path, getuid(), &c, &why) == -1 && c.grace_seconds == 45);
    // The shipped file is valid and states the default.
    FILE *shipped = fopen("config/broker.conf", "r"); assert(shipped);
    char text[MAX_CONFIG + 1]; size_t size = fread(text, 1, MAX_CONFIG, shipped); fclose(shipped);
    text[size] = 0; c.grace_seconds = 0;
    assert(parse_config(text, &c, &why) == 0 && c.grace_seconds == 45);
    assert(unlink(link_path) == 0 && unlink(path) == 0 && rmdir(directory) == 0);
    puts("PASS strict broker.conf parsing: good values load; missing, invalid or unsafe files use defaults");
}
static void test_cleanup_grace(void) {
    for (int ignores = 0; ignores < 2; ignores++) {
        int ready[2], saved[2]; assert(pipe(ready) == 0 && pipe(saved) == 0);
        pid_t worker = fork(); assert(worker >= 0);
        if (!worker) {
            close(ready[0]); close(saved[0]); assert(setsid() >= 0);
            pid_t descendant = fork(); assert(descendant >= 0);
            if (!descendant) {
                sigset_t term; sigemptyset(&term); sigaddset(&term, SIGTERM);
                if (ignores) signal(SIGTERM, SIG_IGN);
                else assert(sigprocmask(SIG_BLOCK, &term, NULL) == 0);
                pid_t self = getpid();
                assert(write(ready[1], &self, sizeof(self)) == (ssize_t)sizeof(self)); close(ready[1]);
                if (ignores) for (;;) pause();
                // An application saving state for a moment after SIGTERM.
                int signo; assert(sigwait(&term, &signo) == 0);
                usleep(300000);
                assert(write(saved[1], "S", 1) == 1); _exit(0);
            }
            // The group leader exits at once and is reaped before cleanup.
            _exit(0);
        }
        close(ready[1]); close(saved[1]); pid_t descendant;
        assert(read_full(ready[0], &descendant, sizeof(descendant)) == 0); close(ready[0]);
        assert(waitpid(worker, NULL, 0) == worker);
        struct session s = {.worker=worker};
        double started = monotonic_seconds(); cleanup(&s);
        double elapsed = monotonic_seconds() - started;
        assert(s.worker == 0);
        char byte; ssize_t n = read(saved[0], &byte, 1); close(saved[0]);
        if (!ignores) assert(n == 1 && byte == 'S');
        // A process ignoring SIGTERM is killed once the grace period ends.
        else assert(n == 0 && elapsed >= 1.5 && elapsed < 5.0);
    }
    puts("PASS logout grants the whole process group a SIGTERM grace period before SIGKILL");
}
int main(void) {
    test_request_bounds(); test_conversation(); test_rollback(); test_fd_transfer(); test_process_cleanup();
    test_auth_failures(); test_launch_failure(); test_group_authorization(); test_peer();
    test_resume_session();
    test_detach_timeout();
    test_desktop_without_audio();
    test_microphone_descriptor();
    test_status_codes(); test_session_limits(); test_config(); test_cleanup_grace();
    assert(valid_id("c42") && !valid_id("../gdm") && !valid_id(""));
    printf("%d native broker groups passed; %d skipped (no PAM/logind session created)\n", 18-peer_skipped, peer_skipped); return 0;
}
