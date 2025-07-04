import os
import sys
import json
import argparse
import numpy as np

path = sys.argv[1]

path2 = None
if len(sys.argv) == 3:
    path2 = sys.argv[2]

def parse_path(path):
    paths = [f"{path}/{f}" for f in os.listdir(path)]

    data = {}
    for path in paths:
        with open(path) as f:
            lines = [line for line in f.readlines()]
            times = lines[2:-2]
            for time in times:
                splits = time.split()
                if len(splits) == 5:
                    percent, seconds, usec_per_call, calls, syscall = splits
                    if syscall not in data:
                        data[syscall] = {"usec_per_call": [], "calls": []}
                    data[syscall]["usec_per_call"].append(float(usec_per_call))
                    data[syscall]["calls"].append(float(calls))

                else:
                    percent, seconds, usec_per_call, calls, errors, syscall = splits
                    if syscall not in data:
                        data[syscall] = {"usec_per_call": [], "calls": []}
                    data[syscall]["usec_per_call"].append(float(usec_per_call))
                    data[syscall]["calls"].append(float(calls))
    return data

def print_data(data):
    for syscall in data:
        info = data[syscall]
        usecs = np.array(info["usec_per_call"])
        calls = np.array(info["calls"])

        usecs_p50 = np.percentile(usecs, 50)
        calls_p50 = np.percentile(calls, 50)
        usecs_p90 = np.percentile(usecs, 90)
        calls_p90 = np.percentile(calls, 90)
        print(f"{syscall:<20}: p50: {usecs_p50:>10.1f} / {calls_p50:>10.1f} p90: {usecs_p90:>10.1f} / {calls_p90:>10.1f}")

def print_delta(data, data2):
    for syscall in data:
        info = data[syscall]
        if syscall not in data2:
            print(f"skipping {syscall}")
            continue
        info2 = data2[syscall]

        usecs = np.array(info["usec_per_call"])
        calls = np.array(info["calls"])

        usecs2 = np.array(info2["usec_per_call"])
        calls2 = np.array(info2["calls"])

        usecs_p50 = np.percentile(usecs, 50)
        calls_p50 = np.percentile(calls, 50)
        usecs_p90 = np.percentile(usecs, 90)
        calls_p90 = np.percentile(calls, 90)

        usecs_p50_2 = np.percentile(usecs2, 50)
        calls_p50_2 = np.percentile(calls2, 50)
        usecs_p90_2 = np.percentile(usecs2, 90)
        calls_p90_2 = np.percentile(calls2, 90)
        print(f"{syscall}:")

        def a(v1, v2):
            diff = v2 - v1
            diff_p = diff / v1 * 100
            print(f"{v1:>10.1f} -> {v2:>10.1f} : {diff:>10.1f} {diff_p:>10.1f}%")

        print("    usecs_p50 ", end="")
        a(usecs_p50, usecs_p50_2)

        print("    calls_p50 ", end="")
        a(calls_p50, calls_p50_2)

        print("    usecs_p90 ", end="")
        a(usecs_p90, usecs_p90_2)

        print("    calls_p90 ", end="")
        a(calls_p90, calls_p90_2)




data = parse_path(path)
print(f"###### {path} #####")
print_data(data)
if path2:
    data2 = parse_path(path2)
    print(f"###### {path2} #####")
    print_data(data2)
    print(f"###### delta #####")
    print_delta(data, data2)

