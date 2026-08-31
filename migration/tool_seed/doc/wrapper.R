# DOC is data entry: the replicates are stored as readings, the chosen standard curve corrects
# them, and the manifest's aggregate outputs (mean, sd) are computed by the engine over the
# curve-applied values, so the preview equals what the database will serve. Nothing is left for
# the script to calculate.

tool <- function(inputs, constants, curves) {
  list()
}
