#!/usr/bin/env python3
"""Build/deploy-time network plan installer; live networking is owned by Rust."""
from importlib.util import module_from_spec, spec_from_file_location
import ipaddress
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import urllib.error
import urllib.request

CONFIG = Path('/etc/nestos/firecracker/config.toml')
PLAN = CONFIG.with_name('network.json')
RUNTIME = Path('/run/nestos-firecracker-network')
STATE = RUNTIME / 'state.json'
INSTALL = Path('/usr/local/libexec/nestos-firecracker')
BLOCKED = ('0.0.0.0/8', '10.0.0.0/8', '100.64.0.0/10', '127.0.0.0/8',
           '169.254.0.0/16', '172.16.0.0/12', '192.0.0.0/24', '192.0.2.0/24',
           '192.168.0.0/16', '198.18.0.0/15', '198.51.100.0/24', '203.0.113.0/24',
           '224.0.0.0/4', '240.0.0.0/4')
SPEC = spec_from_file_location('installer', Path(__file__).with_name('install-environment.py'))
installer = module_from_spec(SPEC)
SPEC.loader.exec_module(installer)


def run(*arguments, namespace=None, data=None, check=True):
    command = list(map(str, arguments))
    if namespace:
        command = ['/usr/sbin/ip', 'netns', 'exec', namespace, *command]
    return subprocess.run(command, input=data, text=True, capture_output=True, check=check)


def save(path, data, mode=0o600):
    installer.atomic_file(path, mode, data=data if isinstance(data, str) else json.dumps(data, indent=2) + '\n')


def api_request(method, path, data=None):
    config = tomllib.loads(CONFIG.read_text())
    values = dict(line.split('=', 1) for line in CONFIG.with_name('environment').read_text().splitlines()
                  if line and not line.startswith('#'))
    key = values[config['api_key_env']].strip().strip('"').strip("'")
    request = urllib.request.Request('http://' + config['listen'] + path, method=method,
        data=None if data is None else json.dumps(data).encode(),
        headers={'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=45) as response:
        return json.load(response)


def service_vms():
    try:
        return api_request('GET', '/v1/vms')['vms']
    except urllib.error.HTTPError:
        raise RuntimeError('cannot authenticate to the running service to check active VMs')
    except urllib.error.URLError as error:
        if getattr(error.reason, 'errno', None) == 111:
            return []
        raise RuntimeError('cannot determine whether VMs are active') from error


def require_empty():
    if service_vms():
        raise RuntimeError('active VMs exist; stop them before changing or stopping their network')


def slot(index, guest, uplink):
    g = ipaddress.ip_network(f'{guest.network_address + index * 256}/30')
    u = ipaddress.ip_network(f'{uplink.network_address + index * 256}/30')
    return {'index': index, 'namespace': f'nestos-fc-{index}', 'tap': 'fc-tap',
            'host_interface': f'mfc-h{index}', 'peer_interface': f'mfc-n{index}',
            'guest_subnet': str(g), 'uplink_subnet': str(u),
            'guest_ip': str(g[2]), 'gateway': str(g[1]),
            'host_ip': str(u[1]), 'peer_ip': str(u[2]),
            'guest_mac': f'02:fc:00:00:00:{index + 1:02x}'}


def dns_servers():
    servers = os.environ.get('FC_DNS_SERVERS', '').split(',')
    if servers == ['']:
        servers = re.findall(r'^nameserver\s+(\S+)', Path('/etc/resolv.conf').read_text(), re.M)
    result = []
    for server in servers:
        address = ipaddress.ip_address(server.strip())
        if address.version == 4 and address.is_global:
            result.append(str(address))
    if not result:
        raise RuntimeError('set FC_DNS_SERVERS to reachable public IPv4 DNS servers')
    return result


def make_plan():
    config = tomllib.loads(CONFIG.read_text())
    guest = ipaddress.ip_network(os.environ.get('FC_GUEST_NETWORK', '10.203.0.0/16'))
    uplink = ipaddress.ip_network(os.environ.get('FC_UPLINK_NETWORK', '10.204.0.0/16'))
    if guest.version != 4 or uplink.version != 4 or guest.prefixlen != 16 or uplink.prefixlen != 16:
        raise RuntimeError('guest and uplink networks must be private IPv4 /16 networks')
    if not guest.is_private or not uplink.is_private or guest.overlaps(uplink):
        raise RuntimeError('guest/uplink networks must be private and must not overlap')
    routes = json.loads(run('/usr/sbin/ip', '-j', '-4', 'route', 'show', 'default').stdout)
    interface = os.environ.get('FC_EGRESS_INTERFACE') or routes[0]['dev']
    if not re.fullmatch(r'[A-Za-z0-9_.:-]{1,15}', interface):
        raise RuntimeError('invalid egress interface')
    return {'schema': 1, 'egress_interface': interface, 'dns_servers': dns_servers(),
            'guest_network': str(guest), 'uplink_network': str(uplink),
            'slots': [slot(i, guest, uplink) for i in range(config['max_vms'])]}


def check_routes(plan):
    networks = [ipaddress.ip_network(plan[name]) for name in ('guest_network', 'uplink_network')]
    routes = json.loads(run('/usr/sbin/ip', '-j', '-4', 'route', 'show', 'table', 'all').stdout)
    for route in routes:
        destination = route.get('dst', 'default')
        if destination == 'default' or route.get('dev', '').startswith('mfc-h'):
            continue
        if any(ipaddress.ip_network(destination).overlaps(network) for network in networks):
            raise RuntimeError(f'network overlaps existing route: {destination}')


def configure_slots(config, plan):
    text = config.read_text()
    if re.search(r'^\[\[profiles\.[^.]+\.network_slots\]\]', text, re.M):
        if PLAN.exists() and json.loads(PLAN.read_text()) == plan:
            return
        raise RuntimeError('network_slots already configured; inspect them before replacing')
    if re.search(r'^\s*network_slots\s*=', text, re.M):
        raise RuntimeError('network_slots already configured inline; inspect them before replacing')
    data = tomllib.loads(text)
    for name in data['profiles']:
        for item in plan['slots']:
            text += f'\n[[profiles.{name}.network_slots]]\n'
            text += f'tap = "{item["tap"]}"\nnetns = "/run/netns/{item["namespace"]}"\n'
            text += f'guest_mac = "{item["guest_mac"]}"\n'
    backup = config.with_name('config.before-network.toml')
    if not backup.exists():
        installer.atomic_file(backup, 0o600, source=config)
    save(config, text)


def install():
    require_empty()
    installer.trusted(CONFIG)
    plan = make_plan()
    check_routes(plan)
    if PLAN.exists() and json.loads(PLAN.read_text()) != plan:
        raise RuntimeError('network plan already exists with different settings; inspect it first')
    configure_slots(CONFIG, plan)
    configure_publisher()
    installer.directory(INSTALL)
    save(PLAN, plan)
    install_dns(plan)
    save(Path('/etc/sysctl.d/90-nestos-firecracker.conf'), 'net.ipv4.ip_forward=1\n', 0o644)
    dns_units = ' '.join(f'nestos-firecracker-dns@{item["index"]}.service' for item in plan['slots'])
    unit = f'[Unit]\nDescription=NestOS microVM network namespaces\nWants=network-online.target {dns_units}\n'
    unit += 'After=network-online.target\nBefore=firecracker-server.service\n[Service]\nType=oneshot\n'
    unit += 'RemainAfterExit=yes\nUMask=0077\nTimeoutStopSec=30\n'
    unit += 'ExecStart=/usr/local/bin/firecracker-server network up\nExecStop=/usr/local/bin/firecracker-server network down\n'
    unit += '[Install]\nWantedBy=multi-user.target\n'
    save(Path('/etc/systemd/system/nestos-firecracker-network.service'), unit, 0o644)
    dropin = Path('/etc/systemd/system/firecracker-server.service.d')
    installer.directory(dropin)
    save(dropin / 'network.conf', f'[Unit]\nRequires=nestos-firecracker-network.service {dns_units}\nAfter=nestos-firecracker-network.service {dns_units}\n', 0o644)
    run('/usr/bin/systemctl', 'daemon-reload')
    print(json.dumps({'network': 'installed', 'slots': len(plan['slots']), 'egress': plan['egress_interface']}))


def configure_publisher():
    text = CONFIG.read_text()
    if 'port_publisher' not in tomllib.loads(text):
        backup = CONFIG.with_name('config.before-publish.toml')
        if not backup.exists():
            save(backup, text)
        save(CONFIG, f'port_publisher = "{INSTALL}/publish-ports"\n' + text)


def install_dns(plan):
    directory = PLAN.with_name('network')
    installer.directory(directory, 0o700)
    for item in plan['slots']:
        options = ['no-resolv', 'bind-interfaces', 'interface=' + item['tap'], 'except-interface=lo',
                   'user=nobody', 'group=nogroup', 'leasefile-ro', 'dhcp-leasefile=/dev/null',
                   f'dhcp-range={item["guest_ip"]},{item["guest_ip"]},255.255.255.252,12h',
                   f'dhcp-host={item["guest_mac"]},{item["guest_ip"]}',
                   f'dhcp-option=option:router,{item["gateway"]}',
                   f'dhcp-option=option:dns-server,{item["gateway"]}']
        options += ['server=' + server for server in plan['dns_servers']]
        save(directory / f'dns-{item["index"]}.conf', '\n'.join(options) + '\n')
    unit = '[Unit]\nDescription=NestOS DHCP/DNS for VM slot %i\n'
    unit += 'Requires=nestos-firecracker-network.service\nAfter=nestos-firecracker-network.service\n'
    unit += 'PartOf=nestos-firecracker-network.service\nBefore=firecracker-server.service\n'
    unit += '[Service]\nType=simple\nRestart=on-failure\nRestartSec=1\n'
    unit += f'ExecStart=/usr/sbin/ip netns exec nestos-fc-%i /usr/sbin/dnsmasq --keep-in-foreground --conf-file={directory}/dns-%i.conf\n'
    save(Path('/etc/systemd/system/nestos-firecracker-dns@.service'), unit, 0o644)


def connection(vm):
    result = run("/usr/local/bin/firecracker-server", "network", "address", vm)
    return result.stdout.split()


if __name__ == '__main__':
    try:
        if os.geteuid() != 0:
            raise RuntimeError('network installation requires root')
        if sys.argv[1:] != ['install']:
            raise RuntimeError('usage: install-network.py install; use firecracker-server network for runtime commands')
        install()
    except Exception as error:
        print('[install-network] result=failed category=' + type(error).__name__, file=sys.stderr)
        sys.exit(1)
