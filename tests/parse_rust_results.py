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
dirs = [d for d in os.listdir(results_path) if os.path.isdir(os.path.join(results_path, d))]

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
    instance_tag = f"{instance_name}_{instance_kernel}"

    block_results_path = os.path.join(instance_results_path, "block")
    results = os.listdir(block_results_path)

    block_data = {}
    for r in results:
        # get test parameters from the file name
        s = r.split("_")
        mode = s[0]
        vcpus = int(s[2])
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
        vcpus = int(s[2])
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
# layout will be:
# {
#   "<instance, kernel>": {
#     "block": [
#       {
#         "mode": "<mode>",
#         ...
#       }
#       ...
#     ],
#     "net": {
#       
#     }
#   }
#
# }

processed_data = {}
for instance in sorted(total_data.keys()):

    processed_data[instance] = {"block": [], "net": []}
    instance_data = total_data[instance]

    block_datas = instance_data["block"]
    for mode in ["read", "randread"]:
        for vcpu in [1, 2, 4]:

            block_data = {"mode": mode, "vcpu": vcpu}
            bw_mean_all = []
            bw_dev_all = []
            iops_mean_all = []
            iops_dev_all = []
            lat_ns_mean_all = []

            for run in ["0", "1"]:
                for bd in block_datas:
                    bw_mean_all.extend(bd[(mode, vcpu, run)]["bw_mean"])
                for bd in block_datas:
                    bw_dev_all.extend(bd[(mode, vcpu, run)]["bw_dev"])
                for bd in block_datas:
                    iops_mean_all.extend(bd[(mode, vcpu, run)]["iops_mean"])
                for bd in block_datas:
                    iops_dev_all.extend(bd[(mode, vcpu, run)]["iops_dev"])
                for bd in block_datas:
                    lat_ns_mean_all.extend(bd[(mode, vcpu, run)]["lat_ns_mean"])
            block_data["bw_mean"] = np.array(bw_mean_all).mean()
            block_data["bw_dev"] = np.array(bw_dev_all).mean()
            block_data["iops_mean"] = np.array(iops_mean_all).mean()
            block_data["iops_dev"] = np.array(iops_dev_all).mean()
            block_data["lat_ns_mean"] = np.array(lat_ns_mean_all).mean()
            processed_data[instance]["block"].append(block_data)

    net_datas = instance_data["net"]
    for mode in ["g2h", "h2g", "bd"]:
        for payload in ["128K", "1024K"]:
            for vcpu in [1, 2, 4]:

                net_data = {"mode": mode, "payload": payload, "vcpu": vcpu}

                if mode != "bd":
                    bps_all = []
                    for worker_id in range(int(vcpu)):
                        for nd in net_datas:
                            bps_all.extend(nd[(mode, payload, vcpu, str(worker_id))]["bits_per_second"])
                    mean = np.array(bps_all).mean()
                    mean = mean / 1000 / 1000 / 1000
                    net_data["bps"] = mean
                else:
                    bps_g2h_all = []
                    bps_h2g_all = []
                    for worker_id in range(int(vcpu)):
                        for nd in net_datas:
                            if worker_id % 2 == 0:
                                bps_g2h_all.extend(nd[(mode, payload, vcpu, str(worker_id))]["bits_per_second"])
                            else:
                                bps_h2g_all.extend(nd[(mode, payload, vcpu, str(worker_id))]["bits_per_second"])
                    if bps_g2h_all:
                        mean = np.array(bps_g2h_all).mean()
                        mean = mean / 1000 / 1000 / 1000
                        net_data["bps_g2h"] = mean

                    if bps_h2g_all:
                        mean = np.array(bps_h2g_all).mean()
                        mean = mean / 1000 / 1000 / 1000
                        net_data["bps_h2g"] = mean
                processed_data[instance]["net"].append(net_data)

print(json.dumps(processed_data, indent=4, sort_keys=True))

