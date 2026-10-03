#!/usr/bin/env python3
"""Local, read-only KVM check. Raw reports belong in ignored data/, not commits."""

import fcntl
import json
import os
from pathlib import Path
import platform
import sys


def inspect():
    flags = set()
    for line in Path('/proc/cpuinfo').read_text().splitlines():
        if line.startswith('flags'):
            flags.update(line.split(':', 1)[1].split())
    result = {
        'kernel': platform.release(),
        'architecture': platform.machine(),
        'logical_cpus': os.cpu_count(),
        'virtualization_flag': bool(flags & {'svm', 'vmx'}),
        'kvm_device': Path('/dev/kvm').exists(),
        'cgroup_v2': Path('/sys/fs/cgroup/cgroup.controllers').exists(),
        'kvm_api_version': None,
        'vm_created': False,
        'blockers': [],
    }
    if not result['virtualization_flag']:
        result['blockers'].append('CPU does not expose svm/vmx; inspect firmware and kernel logs.')
    try:
        fd = os.open('/dev/kvm', os.O_RDWR | os.O_CLOEXEC)
        try:
            result['kvm_api_version'] = fcntl.ioctl(fd, 0xAE00, 0)
            if result['kvm_api_version'] != 12:
                result['blockers'].append('Expected KVM API version 12.')
            else:
                vm_fd = fcntl.ioctl(fd, 0xAE01, 0)
                os.close(vm_fd)
                result['vm_created'] = True
        finally:
            os.close(fd)
    except OSError as error:
        result['blockers'].append(f'KVM unavailable: {error.strerror} (errno {error.errno}).')
    result['ready_for_boot_experiment'] = not result['blockers']
    return result


if __name__ == '__main__':
    report = inspect()
    print(json.dumps(report, indent=2))
    sys.exit(0 if report['ready_for_boot_experiment'] else 2)
