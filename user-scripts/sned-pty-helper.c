#include <sys/ioctl.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char *argv[]) {
    if (argc < 3) {
        fprintf(stderr, "Usage: sned-pty-helper ROWS COLS command [args...]\n");
        return 1;
    }
    int rows = atoi(argv[1]);
    int cols = atoi(argv[2]);
    struct winsize ws;
    ws.ws_row = rows;
    ws.ws_col = cols;
    ws.ws_xpixel = 0;
    ws.ws_ypixel = 0;
    if (ioctl(STDIN_FILENO, TIOCSWINSZ, &ws) != 0) {
        /* Not a tty — best effort, the child may still work */
    }
    execvp(argv[3], &argv[3]);
    perror("execvp");
    return 1;
}
