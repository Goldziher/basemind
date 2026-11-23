terraform {
  required_version = ">= 1.5"

  required_providers {
    aws = {
      source = "hashicorp/aws"
    }
  }
}

provider "aws" {
  region = var.region
}

variable "region" {
  type    = string
  default = "eu-west-1"
}

locals {
  prefix = "app"
  tags = {
    owner = "platform"
  }
}

data "aws_iam_policy_document" "assume" {
  statement {
    actions = ["sts:AssumeRole"]
  }
}

resource "aws_s3_bucket" "logs" {
  bucket = "${local.prefix}-logs"

  lifecycle {
    prevent_destroy = true
  }
}

module "network" {
  source = "./modules/network"
}

output "bucket_arn" {
  value = aws_s3_bucket.logs.arn
}
