load("//tools:defs.bzl", "my_rule", helper = "helper_fn")
load("@rules_python//python:defs.bzl", "py_library")
load(":local.bzl", "local_rule")

DEPS = ["//a:b"]
_private = 1

def macro(name, srcs = []):
    inner = 1
    native.cc_library(name = name, srcs = srcs)

py_library(
    name = "lib",
    srcs = ["a.py"],
)

cc_binary(name = "tool", srcs = ["t.cc"])
x = my_rule(name = "assigned")
exports_files(["data.txt"])
