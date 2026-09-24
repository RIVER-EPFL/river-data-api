# Each case runs in a fork, because the filter a run installs stays on the process for good.
in_fork <- function(expr) {
  job <- parallel::mcparallel(expr)
  parallel::mccollect(job)[[1]]
}

test_that("a run cannot open a network socket and still computes", {
  port <- 38000L + Sys.getpid() %% 1000L
  server <- serverSocket(port)
  on.exit(close(server))
  script <- sprintf(
    "tool <- function(inputs, constants, curves) { socketConnection('127.0.0.1', %d, open = 'r+b', timeout = 2); list(v = 1) }",
    port
  )
  seen <- in_fork({
    refused <- tryCatch({ run_tool(script); FALSE }, error = function(e) TRUE)
    doubled <- run_tool("tool <- function(inputs, constants, curves) list(v = inputs$x * 2)", inputs = list(x = 21))$v
    list(refused = refused, doubled = doubled)
  })
  expect_true(seen$refused)
  expect_equal(seen$doubled, 42)
})

test_that("the filter stays in the run that installed it", {
  port <- 39000L + Sys.getpid() %% 1000L
  server <- serverSocket(port)
  on.exit(close(server))
  in_fork(deny_network())
  client <- socketConnection("127.0.0.1", port, open = "r+b", timeout = 2)
  expect_true(isOpen(client))
  close(client)
})
