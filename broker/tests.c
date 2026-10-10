#define _GNU_SOURCE
#include <assert.h>
#include <security/pam_appl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static int close_count, delete_count, end_count, order;
static int peer_skipped;
static int fail_step;
static struct pam_conv saved_conversation;
static int fake_start(const char *service, const char *user, const struct pam_conv *conv, pam_handle_t **pam) {
    assert(!strcmp(service, "niri-rdp") && !strcmp(user, "alice"));
    saved_conversation = *conv;
    if (fail_step == 0) { *pam = NULL; return PAM_SYSTEM_ERR; }
    *pam = (pam_handle_t *)1; return PAM_SUCCESS;
}
static int fake_item(pam_handle_t *pam, int item, const void *value) {
    assert(pam && item == PAM_RHOST && !strcmp(value, "rdp-loopback")); return PAM_SUCCESS;
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
static int fake_get_item(const pam_handle_t *pam, int item, const void **value) {
    assert(pam && item == PAM_USER); *value = "root"; return PAM_SUCCESS;
}
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
#define WORKER "/nonexistent/niri-rdp-worker-test"
#define BROKER_TEST
#include "broker.c"

static void reset_counts(void) { close_count = delete_count = end_count = order = 0; }
static void test_request_bounds(void) {
    int p[2]; assert(socketpair(AF_UNIX, SOCK_STREAM, 0, p) == 0);
    unsigned char good[] = {'N','R','B','1',0,5,0,4,'a','l','i','c','e','t','e','s','t'};
    assert(write(p[0], good, sizeof(good)) == (ssize_t)sizeof(good));
    struct request r; assert(read_request(p[1], &r) == 0);
    assert(!strcmp(r.user, "alice") && !strcmp(r.password, "test"));
    wipe_request(&r); for (size_t i = 0; i < sizeof(r); i++) assert(((char *)&r)[i] == 0);
    unsigned char large[] = {'N','R','B','1',0,65,0,1};
    assert(write(p[0], large, sizeof(large)) == (ssize_t)sizeof(large));
    assert(read_request(p[1], &r) == -1);
    close(p[0]); close(p[1]);
    for (unsigned int scenario = 0; scenario < 5; scenario++) {
        assert(socketpair(AF_UNIX, SOCK_STREAM, 0, p) == 0);
        unsigned char bad[] = {'N','R','B','1',0,1,0,1,'a','x'};
        if (scenario == 0) bad[0] = 'X';
        if (scenario == 1) bad[5] = 0;
        if (scenario == 2) bad[8] = 0;
        if (scenario == 3) bad[9] = 0;
        if (scenario == 4) bad[8] = '/';
        assert(write(p[0], bad, sizeof(bad)) == (ssize_t)sizeof(bad)); shutdown(p[0], SHUT_WR);
        assert(read_request(p[1], &r) == -1); close(p[0]); close(p[1]);
    }
    puts("PASS bounded protocol, malformed identity, zeroized credentials");
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
    assert(!memcmp(bytes, "NRB1\0", 5));
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
static void test_auth_failures(void) {
    for (fail_step = 0; fail_step < 4; fail_step++) {
        reset_counts(); int pair[2], notification[2];
        assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0); assert(pipe(notification) == 0);
        unsigned char request[] = {'N','R','B','1',0,5,0,4,'a','l','i','c','e','t','e','s','t'};
        assert(write(pair[0], request, sizeof(request)) == (ssize_t)sizeof(request));
        handle_connection(pair[1], notification[1]);
        unsigned char response[5]; assert(read_full(pair[0], response, 5) == 0);
        assert(!memcmp(response, "NRB1", 4) && response[4] == 1);
        assert(!close_count && !delete_count && end_count == (fail_step > 0));
        char byte; assert(read(notification[0], &byte, 1) == 0);
        close(pair[0]); close(notification[0]);
    }
    puts("PASS PAM init/auth/account failure and final PAM identity authorization reject without sessions");
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
int main(void) {
    test_request_bounds(); test_conversation(); test_rollback(); test_fd_transfer(); test_process_cleanup();
    test_auth_failures(); test_launch_failure(); test_peer();
    assert(valid_id("c42") && !valid_id("../gdm") && !valid_id(""));
    printf("%d native broker groups passed; %d skipped (no PAM/logind session created)\n", 9-peer_skipped, peer_skipped); return 0;
}
