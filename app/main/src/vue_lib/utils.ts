import { Visibility } from '@martichou/core_lib/bindings/Visibility';
import { TauriVM } from './helper/ParamsHelper';
import { autostartKey, deviceNameKey, trustedKey, DisplayedItem, downloadPathKey, numberToVisibility, realcloseKey, startminimizedKey, stateToDisplay, visibilityKey, visibilityToNumber } from './types';
import { SendInfo } from '@martichou/core_lib/bindings/SendInfo';
import { ChannelMessage } from '@martichou/core_lib/bindings/ChannelMessage';
import { ChannelAction } from '@martichou/core_lib';
import { gt } from 'semver';

function _displayedItems(vm: TauriVM): Array<DisplayedItem> {
	const ndisplayed = new Array<DisplayedItem>();

	vm.endpointsInfo.forEach((el) => {
		const idx = ndisplayed.findIndex((nel) => el.id == nel.id);
		if (idx !== -1) return;

		const card: DisplayedItem = {
			id: el.id,
			name: el.name ?? 'Unknown',
			deviceType: el.rtype ?? 'Unknown',
			endpoint: true,
			connecting: vm.connectingId === el.id,
		};

		// One device, one card: the same phone is often discovered over both
		// mDNS (ip:port) and BLE (ble://name). Prefer the Wi-Fi entry — direct
		// TCP beats a BLE handshake plus upgrade dance — and if the Wi-Fi one
		// goes stale (device left the network) its failed connect removes it,
		// revealing the BLE card again.
		const dupIdx = ndisplayed.findIndex((nel) => nel.endpoint && nel.name === card.name);
		if (dupIdx !== -1) {
			// Never swap the card's identity while a connection to it is in
			// flight: the user's next click would fire a SECOND, parallel send
			// over the other transport, and two simultaneous sessions wedge
			// the phone's Nearby stack.
			if (vm.connectingId === ndisplayed[dupIdx].id) return;
			const existing = vm.endpointsInfo.find((e) => e.id === ndisplayed[dupIdx].id);
			const existingIsBle = !!existing?.ble_addr;
			const thisIsBle = !!el.ble_addr;
			if (existingIsBle && !thisIsBle) {
				ndisplayed.splice(dupIdx, 1, card); // Wi-Fi replaces BLE
			}
			return; // never show the same device twice
		}

		ndisplayed.push(card);
	});

	vm.requests.filter((el) => stateToDisplay.includes(el.state ?? 'Initial')).forEach((el) => {
		const idx = ndisplayed.findIndex((nel) => el.id == nel.id);
		const elem: DisplayedItem = {
			id: el.id,
			// A failure before the handshake carries no metadata; the endpoint
			// list still knows who was clicked.
			name: el.meta?.source?.name
				?? vm.endpointsInfo.find((e) => e.id === el.id)?.name
				?? 'Unknown',
			deviceType: el.meta?.source?.device_type ?? 'Unknown',
			endpoint: false,

			state: el.state ?? undefined,
			pin_code: el.meta?.pin_code ?? undefined,
			destination: el.meta?.destination ?? undefined,
			files: el.meta?.files ?? undefined,
			text_description: el.meta?.text_description ?? undefined,
			text_payload: el.meta?.text_payload ?? undefined,
			text_type: el.meta?.text_type ?? undefined,
			ack_bytes: (el.meta?.ack_bytes as number | undefined) ?? undefined,
			total_bytes: (el.meta?.total_bytes as number | undefined) ?? undefined,
		};

		if (idx !== -1) {
			ndisplayed.splice(idx, 1, elem);
		} else {
			ndisplayed.push(elem)
		}
	});

	return ndisplayed;
}

async function setAutoStart(vm: TauriVM, autostart: boolean) {
	if (autostart) {
		await vm.enable();
	} else {
		await vm.disable();
	}

	await vm.store.set(autostartKey, autostart);
	await vm.store.save();
	vm.autostart = autostart;
}

async function applyAutoStart(vm: TauriVM) {
	vm.autostart = await vm.store.get(autostartKey) ?? false;

	if (vm.autostart) {
		await vm.enable();
	} else {
		await vm.disable();
	}
}

async function setRealClose(vm: TauriVM, realclose: boolean) {
	await vm.store.set(realcloseKey, realclose);
	await vm.store.save();
	vm.realclose = realclose;
}

async function getRealclose(vm: TauriVM) {
	vm.realclose = await vm.store.get(realcloseKey) ?? false;
}

async function setStartMinimized(vm: TauriVM, startminimized: boolean) {
	await vm.store.set(startminimizedKey, startminimized);
	await vm.store.save();
	vm.startminimized = startminimized;
}

async function getStartMinimized(vm: TauriVM) {
	vm.startminimized = await vm.store.get(startminimizedKey) ?? false;
}

async function setVisibility(vm: TauriVM, visibility: Visibility) {
	await vm.invoke('change_visibility', { message: visibility });
	await vm.store.set(visibilityKey, visibilityToNumber[visibility]);
	await vm.store.save();
	vm.visibility = visibility;
}

async function getVisibility(vm: TauriVM) {
	vm.visibility = numberToVisibility[(await vm.store.get(visibilityKey) ?? 0) as number];
}

async function invertVisibility(vm: TauriVM) {
	if (vm.visibility === 'Temporarily') {
		return;
	}

	if (vm.visibility === 'Visible') {
		return await vm.setVisibility(vm, 'Invisible');
	}

	return await vm.setVisibility(vm, 'Visible');
}

async function clearSending(vm: TauriVM, ) {
	await vm.invoke('stop_discovery');
	vm.outboundPayload = undefined;
	vm.discoveryRunning = false;
	vm.endpointsInfo = [];
	vm.connectingId = null;
}

function removeRequest(vm: TauriVM, id: string) {
	const idx = vm.requests.findIndex((el) => el.id === id);

	if (idx !== -1) {
		vm.requests.splice(idx, 1);
	}
}

async function sendInfo(vm: TauriVM, eid: string) {
	if (vm.outboundPayload === undefined) return;

	// One outbound at a time: a second click while a connect/handshake is in
	// flight launches a parallel session that wedges the phone's Nearby stack
	// (drops right after paired-key until it recovers). connectingId clears
	// on the transfer's first state event or its failure.
	if (vm.connectingId !== null) return;

	const ei = vm.endpointsInfo.find((el) => el.id === eid);
	if (!ei) return;

	// A recipient discovered over Bluetooth carries a `ble_addr` instead of an
	// ip/port; send it over BLE (the library dials and upgrades from there).
	const isBle = !!(ei as any).ble_addr;
	if (!isBle && (!ei.ip || !ei.port)) return;

	const msg = {
		id: ei.id,
		name: ei.name ?? 'Unknown',
		addr: (ei.ip && ei.port) ? (ei.ip + ":" + ei.port) : "",
		ob: vm.outboundPayload,
		ble: isBle,
	} as SendInfo;

	// Immediate feedback: a BLE connect can take seconds (scan + dial), and a
	// click with no visible reaction reads as a dead button.
	vm.connectingId = ei.id;

	await vm.invoke('send_payload', { message: msg });
}

// Devices whose incoming transfers are accepted without asking. Stored as a
// plain list of device names — the only stable identity Quick Share exposes.
async function getTrusted(vm: TauriVM): Promise<string[]> {
	return ((await vm.store.get(trustedKey)) as string[] | undefined) ?? [];
}

async function addTrusted(vm: TauriVM, name: string): Promise<string[]> {
	const list = await getTrusted(vm);
	if (!list.includes(name)) list.push(name);
	await vm.store.set(trustedKey, list);
	await vm.store.save();
	return list;
}

async function removeTrusted(vm: TauriVM, name: string): Promise<string[]> {
	const list = (await getTrusted(vm)).filter((n) => n !== name);
	await vm.store.set(trustedKey, list);
	await vm.store.save();
	return list;
}

async function sendCmd(vm: TauriVM, id: string, action: ChannelAction) {
	const cm: ChannelMessage = {
		id: id,
		direction: 'FrontToLib',
		action: action,
		meta: null,
		state: null,
		rtype: null,
	};
	console.log("js2rs:", cm);

	await vm.invoke('send_to_rs', { message: cm });
}

function blured() {
	(document.activeElement as any).blur();
}

function fmtBytes(n: number): string {
	if (!Number.isFinite(n) || n < 0) return '';
	const units = ['B', 'KB', 'MB', 'GB', 'TB'];
	let v = n, i = 0;
	while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
	return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

function getProgress(item: DisplayedItem): string {
	const value = item.ack_bytes! / item.total_bytes! * 100;
	return `--progress: ${value}`;
}

async function setDownloadPath(vm: TauriVM, dest: string) {
	await vm.invoke('change_download_path', { message: dest });
	await vm.store.set(downloadPathKey, dest);
	await vm.store.save();
	vm.downloadPath = dest;
}

async function getDownloadPath(vm: TauriVM) {
	vm.downloadPath = await vm.store.get(downloadPathKey) ?? undefined;
}

async function setDeviceName(vm: TauriVM, name: string) {
	const trimmed = name.trim();
	// An empty name resets to the OS hostname.
	const finalName = trimmed.length ? trimmed : (await vm.invoke('get_hostname') as string);

	await vm.invoke('set_device_name', { name: finalName });
	await vm.store.set(deviceNameKey, trimmed.length ? trimmed : null);
	await vm.store.save();
	vm.hostname = finalName;
}

async function getDeviceName(vm: TauriVM) {
	// The lib holds the effective name: the custom one applied at startup, or
	// the OS hostname when none was set.
	vm.hostname = await vm.invoke('get_device_name') as string;
}

export type Theme = 'light' | 'dark' | 'system';

function applyTheme(theme: Theme) {
	let dark = theme === 'dark';
	if (theme === 'system') {
		try {
			dark = window.matchMedia('(prefers-color-scheme: dark)').matches;
		} catch { /* default light */ }
	}
	document.documentElement.classList.toggle('dark', dark);
}

async function setTheme(vm: TauriVM, theme: Theme) {
	try {
		localStorage.setItem('theme', theme);
	} catch { /* ignore */ }
	applyTheme(theme);
	vm.theme = theme;
}

function getTheme(vm: TauriVM) {
	let theme: Theme = 'system';
	try {
		theme = (localStorage.getItem('theme') as Theme | null) ?? 'system';
	} catch { /* ignore */ }
	vm.theme = theme;
	applyTheme(theme);
}

async function getLatestVersion(vm: TauriVM) {
	try {
		const response = await fetch('https://api.github.com/repos/ignotusbucius/open-quickshare/releases/latest');
		if (!response.ok) {
			throw new Error(`Error: ${response.status} ${response.statusText}`);
		}
		const data = await response.json();
		const new_version = data.tag_name.substring(1);
		console.log(`Latest version: ${vm.version} vs ${new_version}`);

		if (vm.version && gt(new_version, vm.version)) {
			vm.new_version = new_version;
		}
	} catch (err) {
		console.error(err);
	}
}

// Default export
export const utils = {
	_displayedItems,
	setAutoStart,
	applyAutoStart,
	setRealClose,
	getRealclose,
	setVisibility,
	getVisibility,
	invertVisibility,
	clearSending,
	removeRequest,
	sendInfo,
	sendCmd,
	blured,
	getProgress,
	setDownloadPath,
	getDownloadPath,
	getLatestVersion,
	setStartMinimized,
	getStartMinimized,
	setDeviceName,
	getDeviceName,
	getTrusted,
	addTrusted,
	removeTrusted,
	setTheme,
	getTheme,
	applyTheme,
	fmtBytes
};
export type UtilsType = typeof utils;