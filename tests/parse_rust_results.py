import os
import sys
import json
import subprocess
import numpy as np


def print_want_big(val, s):
    if val < 0.0:
        print_red(s)
    else:
        print_green(s)

def print_want_small(val, s):
    if val > 0.0:
        print_red(s)
    else:
        print_green(s)

def print_red(s): print("\033[91m {}\033[00m" .format(s))
def print_green(s): print("\033[92m {}\033[00m" .format(s))


results_path = sys.argv[1]

dirs = [d for d in os.listdir(results_path) if os.path.isdir(d)]

# each dir has a structure
# rust_test_results
# |- instance (file with instance/kernel info)
# |- net (dir with net perf results)
# |- block (dir with block perf results)

total_data = {}

# parse all the data
for d in dirs:
    dir_path = os.path.join(results_path, d)

    instance_results_path = os.path.join(dir_path, "rust_test_results")

    instance_info_path = os.path.join(instance_results_path, "instance")
    file = open(instance_info_path)
    instance = file.readlines()
    instance_info = instance[0].split('/')
    instance_name = instance_info[0]
    instance_kernel = instance_info[1][0]
    # used as a key to instance data
    instance_tag = (instance_name, instance_kernel)

    block_results_path = os.path.join(instance_results_path, "block")
    results = os.listdir(block_results_path)

    block_data = {}
    for r in results:
        # get test parameters from the file name
        s = r.split("_")
        mode = s[0]
        vcpus = s[2]
        run = os.path.splitext(s[4])[0]

        bw_mean = []
        bw_dev = []
        iops_mean = []
        iops_dev = []
        lat_ns_mean = []

        file_path = os.path.join(block_results_path, r)
        file = open(file_path)
        try:
            j = json.load(file)
            for job in j["jobs"]:
                bw_mean.append(job["read"]["bw_mean"])
                bw_dev.append(job["read"]["bw_dev"])
                iops_mean.append(job["read"]["iops_mean"])
                iops_dev.append(job["read"]["iops_stddev"])
                lat_ns_mean.append(job["read"]["lat_ns"]["mean"])
        except:
            print("could not parse json")

        block_data[(mode, vcpus, run)] = { "bw_mean": bw_mean, "bw_dev": bw_dev, "iops_mean": iops_mean, "iops_dev": iops_dev, "lat_ns_mean": lat_ns_mean }

    net_results_path = os.path.join(instance_results_path, "net")
    results = os.listdir(net_results_path)

    net_data = {}
    for r in results:
        # get test parameters from the file name
        s = r.split("_")
        mode = s[0]
        payload = s[1]
        vcpus = s[2]
        worker_id = os.path.splitext(s[3])[0]

        bits_per_second = []

        file_path = os.path.join(net_results_path, r)
        file = open(file_path)
        try:
            j = json.load(file)
            for interval in j["intervals"]:
                # skip omitted ones
                if interval["sum"]["omitted"] == True:
                    continue
                bits_per_second.append(interval["sum"]["bits_per_second"])
        except:
            print("could not parse json")

        net_data[(mode, payload, vcpus, worker_id)] = { "bits_per_second": bits_per_second }


    if instance_tag in total_data.keys():
        total_data[instance_tag]["block"].append(block_data)
        total_data[instance_tag]["net"].append(net_data)
    else:
        total_data[instance_tag] = {
                "block": [block_data],
                "net": [net_data],
        }

# output all the data
for instance in sorted(total_data.keys()):
    print(f"instance: {instance}")
    instance_data = total_data[instance]

    block_datas = instance_data["block"]
    for mode in ["read", "randread"]:
        for vcpu in ["1", "2", "4"]:
            for run in ["0", "1"]:
                print(f"block: mode: {mode}, vcpu: {vcpu}, run: {run}")
                bw_mean_all = []
                for block_data in block_datas:
                    bw_mean_all.extend(block_data[(mode, vcpu, run)]["bw_mean"])
                mean = np.array(bw_mean_all).mean()
                print(f"bw_mean mean: {mean:.2f} Mb/s")

                bw_dev_all = []
                for block_data in block_datas:
                    bw_mean_all.extend(block_data[(mode, vcpu, run)]["bw_dev"])
                mean = np.array(bw_mean_all).mean()
                print(f"bw_dev mean: {mean:.2f} Mb/s")

                iops_mean_all = []
                for block_data in block_datas:
                    bw_mean_all.extend(block_data[(mode, vcpu, run)]["iops_mean"])
                mean = np.array(bw_mean_all).mean()
                print(f"iops_mean mean: {mean:.2f}")

                iops_dev_all = []
                for block_data in block_datas:
                    bw_mean_all.extend(block_data[(mode, vcpu, run)]["iops_dev"])
                mean = np.array(bw_mean_all).mean()
                print(f"iops_dev mean: {mean:.2f}")

                lat_ns_mean_all = []
                for block_data in block_datas:
                    bw_mean_all.extend(block_data[(mode, vcpu, run)]["lat_ns_mean"])
                mean = np.array(bw_mean_all).mean()
                print(f"lat_ns_mean mean: {mean:.2f} ns")

    net_datas = instance_data["net"]
    for mode in ["g2h", "h2g", "bd"]:
        for payload in ["128K", "1024K"]:
            for vcpu in ["1", "2", "4"]:
                print(f"net: mode: {mode}, payload: {payload}, vcpu: {vcpu}");

                if mode != "bd":
                    bps_all = []
                    for worker_id in range(int(vcpu)):
                        for net_data in net_datas:
                            bps_all.extend(net_data[(mode, payload, vcpu, str(worker_id))]["bits_per_second"])
                    mean = np.array(bps_all).mean()
                    mean = mean / 1000 / 1000 / 1000
                    print(f"bps {mode} mean: {mean:.2f} Gb/s")
                else:
                    bps_g2h_all = []
                    bps_h2g_all = []
                    for worker_id in range(int(vcpu)):
                        for net_data in net_datas:
                            if worker_id % 2 == 0:
                                bps_g2h_all.extend(net_data[(mode, payload, vcpu, str(worker_id))]["bits_per_second"])
                            else:
                                bps_h2g_all.extend(net_data[(mode, payload, vcpu, str(worker_id))]["bits_per_second"])
                    mean = np.array(bps_g2h_all).mean()
                    mean = mean / 1000 / 1000 / 1000
                    print(f"bps g2h mean: {mean:.2f} Gb/s")

                    mean = np.array(bps_h2g_all).mean()
                    mean = mean / 1000 / 1000 / 1000
                    print(f"bps h2g mean: {mean:.2f} Gb/s")

