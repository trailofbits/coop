#define _GNU_SOURCE
#include <errno.h>
#include <pwd.h>
#include <security/pam_appl.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <termios.h>
#include <unistd.h>

/* Unprivileged PAM client: unlocks this UID's existing service, never authenticates
 * a login or starts a daemon. The parent verifies storage and Secret Service.
 * Keep passwords out of the Python coordinator, argv, environment and files. */
static volatile sig_atomic_t cancelled;
static char password[512];
static char confirmation[512];

static void cancel(int sig) { cancelled = sig; }

static bool prompt(const char *label, char *out, size_t capacity) {
    struct termios saved, hidden;
    if (tcgetattr(STDIN_FILENO, &saved) != 0) return false;
    hidden = saved;
    hidden.c_lflag &= ~(ECHO | ECHONL);
    if (tcsetattr(STDIN_FILENO, TCSAFLUSH, &hidden) != 0) return false;
    fputs(label, stderr);
    fflush(stderr);
    size_t used = 0;
    bool ok = false;
    while (!cancelled) {
        char c = 0;
        ssize_t n = read(STDIN_FILENO, &c, 1);
        if (n != 1) break;
        if (c == '\n') { ok = used > 0; break; }
        if (c == '\0' || used + 1 >= capacity) break;
        out[used++] = c;
    }
    out[used] = '\0';
    if (tcsetattr(STDIN_FILENO, TCSAFLUSH, &saved) != 0) ok = false;
    fputc('\n', stderr);
    return ok && !cancelled;
}

static int converse(int count, const struct pam_message **messages,
                    struct pam_response **out, void *unused) {
    (void)unused;
    if (cancelled || count != 1 || messages[0]->msg_style != PAM_PROMPT_ECHO_OFF)
        return PAM_CONV_ERR;
    struct pam_response *responses = calloc(1, sizeof(*responses));
    if (!responses) return PAM_BUF_ERR;
    responses[0].resp = strdup(password);
    if (!responses[0].resp) { free(responses); return PAM_BUF_ERR; }
    /* libpam owns and wipes this copy when it releases the response/token. */
    *out = responses;
    return PAM_SUCCESS;
}

int main(int argc, char **argv) {
    bool creating = argc == 2 && strcmp(argv[1], "--create") == 0;
    if (argc != 1 && !creating) return 2;
    if (geteuid() != getuid() || getuid() == 0 || !isatty(STDIN_FILENO)) {
        fputs("keyring unlock requires the guest user's interactive TTY\n", stderr);
        return 2;
    }
    if (prctl(PR_SET_DUMPABLE, 0) != 0 ||
        mlock(password, sizeof(password)) != 0 ||
        mlock(confirmation, sizeof(confirmation)) != 0) return 2;
    struct sigaction action = {.sa_handler = cancel};
    sigemptyset(&action.sa_mask);
    const int signals[] = {SIGINT, SIGTERM, SIGHUP, SIGALRM};
    for (size_t i = 0; i < sizeof(signals) / sizeof(signals[0]); i++)
        if (sigaction(signals[i], &action, NULL) != 0) return 2;
    alarm(120);
    int result = 1;
    pam_handle_t *handle = NULL;
    if (creating)
        fputs("Choose a nonempty password for the guest's encrypted keyring.\n", stderr);
    if (!prompt("Codex keyring password: ", password, sizeof(password))) goto cleanup;
    if (creating) {
        if (!prompt("Confirm keyring password: ", confirmation, sizeof(confirmation))) goto cleanup;
        if (strcmp(password, confirmation) != 0) {
            fputs("Passwords did not match; no keyring was created.\n", stderr);
            goto cleanup;
        }
    }
    explicit_bzero(confirmation, sizeof(confirmation));
    struct passwd *user = getpwuid(getuid());
    if (!user) goto cleanup;
    struct pam_conv conv = {converse, NULL};
    int status = pam_start("coop-codex-keyring", user->pw_name, &conv, &handle);
    if (status != PAM_SUCCESS) goto cleanup;
    char control[128];
    int length = snprintf(control, sizeof(control), "GNOME_KEYRING_CONTROL=/run/user/%lu/keyring",
                          (unsigned long)getuid());
    if (length < 0 || (size_t)length >= sizeof(control)) goto cleanup;
    status = pam_putenv(handle, control);
    alarm(30);
    if (status == PAM_SUCCESS && !cancelled) status = pam_authenticate(handle, 0);
    if (status == PAM_SUCCESS && !cancelled) result = 0;
cleanup:
    if (handle) pam_end(handle, result == 0 ? PAM_SUCCESS : PAM_ABORT);
    explicit_bzero(password, sizeof(password));
    explicit_bzero(confirmation, sizeof(confirmation));
    munlock(password, sizeof(password));
    munlock(confirmation, sizeof(confirmation));
    alarm(0);
    if (cancelled) fputs("Keyring unlock cancelled.\n", stderr);
    return cancelled ? 128 + cancelled : result;
}
