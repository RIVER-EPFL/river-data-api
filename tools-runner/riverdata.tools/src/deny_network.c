#include <errno.h>
#include <stddef.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>

#include <R.h>
#include <Rinternals.h>
#include <R_ext/Rdynload.h>

#if !defined(__x86_64__)
#error "the socket filter is written for x86_64 syscall numbers"
#endif

/* The calling process loses network sockets for the rest of its life: socket() for any domain but
 * AF_UNIX and io_uring_setup return EACCES, and a syscall entered through any other ABI (i386,
 * x32, where socketcall reaches the same thing) kills the process. */
SEXP deny_network(void) {
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JGE | BPF_K, __X32_SYSCALL_BIT, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_io_uring_setup, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (EACCES & SECCOMP_RET_DATA)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_socket, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AF_UNIX, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (EACCES & SECCOMP_RET_DATA)),
    };
    struct sock_fprog program = {
        .len = (unsigned short) (sizeof(filter) / sizeof(filter[0])),
        .filter = filter,
    };
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) {
        Rf_error("cannot set no_new_privs: %s", strerror(errno));
    }
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program) != 0) {
        Rf_error("cannot install the socket filter: %s", strerror(errno));
    }
    return R_NilValue;
}

static const R_CallMethodDef CALLS[] = {
    {"deny_network", (DL_FUNC) &deny_network, 0},
    {NULL, NULL, 0},
};

void R_init_riverdata_tools(DllInfo *dll) {
    R_registerRoutines(dll, NULL, CALLS, NULL, NULL);
    R_useDynamicSymbols(dll, FALSE);
}
