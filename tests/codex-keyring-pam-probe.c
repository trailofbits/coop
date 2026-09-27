#define _GNU_SOURCE
#include <security/pam_appl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Test helper for test-codex-desktop-prototype.py, not a login authenticator.
 * Uses a temporary PAM config directory; never changes the system PAM stack.
 * Read only disposable fixture passwords from stdin, never argv or env. */
static int converse(int count, const struct pam_message **messages,
                    struct pam_response **out, void *unused) {
    (void)unused;
    if (count != 1 || messages[0]->msg_style != PAM_PROMPT_ECHO_OFF)
        return PAM_CONV_ERR;
    char *password = NULL;
    size_t capacity = 0;
    ssize_t length = getline(&password, &capacity, stdin);
    if (length < 0) { free(password); return PAM_CONV_ERR; }
    if (length && password[length - 1] == '\n') password[--length] = '\0';
    if (!length) { free(password); return PAM_CONV_ERR; }
    *out = calloc(1, sizeof(struct pam_response));
    if (!*out) { explicit_bzero(password, capacity); free(password); return PAM_BUF_ERR; }
    (*out)->resp = password;
    return PAM_SUCCESS;
}

int main(int argc, char **argv) {
    if (argc != 4) return 2;
    struct pam_conv conv = {converse, NULL};
    pam_handle_t *handle = NULL;
    int result = pam_start_confdir("coop-keyring", argv[1], &conv, argv[2], &handle);
    if (result != PAM_SUCCESS) return 3;
    char *environment = NULL;
    if (asprintf(&environment, "GNOME_KEYRING_CONTROL=%s", argv[3]) < 0) {
        pam_end(handle, PAM_BUF_ERR);
        return 4;
    }
    result = pam_putenv(handle, environment);
    free(environment);
    if (result == PAM_SUCCESS) result = pam_authenticate(handle, 0);
    printf("PAM result: %d\n", result);
    pam_end(handle, result);
    return result == PAM_SUCCESS ? 0 : 1;
}
