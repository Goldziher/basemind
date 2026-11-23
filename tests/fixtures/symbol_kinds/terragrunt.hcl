include "root" {
  path = find_in_parent_folders()
}

dependency "vpc" {
  config_path = "../vpc"
}

terraform {
  source = "../modules//app"
}

locals {
  env = "prod"
}

inputs = {
  vpc_id = dependency.vpc.outputs.vpc_id
}
