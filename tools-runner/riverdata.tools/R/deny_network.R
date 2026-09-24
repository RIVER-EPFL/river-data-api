#' Take network sockets away from this process for good.
#'
#' Installs a seccomp filter under which `socket()` for any domain but `AF_UNIX` fails with
#' `EACCES`. OpenCPU forks a process per request, so a run loses its network and the server keeps
#' its own.
deny_network <- function() {
  invisible(.Call(C_deny_network))
}
