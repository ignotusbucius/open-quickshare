<script setup lang="ts">
import { utils } from '../vue_lib';
import { PropType, ref, watchEffect } from 'vue';
import { TauriVM } from '../vue_lib/helper/ParamsHelper';

const props = defineProps({
	vm: {
		type: Object as PropType<TauriVM>,
		required: true
	}
});

const trusted = ref<string[]>([]);
watchEffect(async () => {
	if (props.vm.settingsOpen) trusted.value = await utils.getTrusted(props.vm);
});
async function dropTrusted(name: string) {
	trusted.value = await utils.removeTrusted(props.vm, name);
}

const emit = defineEmits(['close']);

function openDownloadPicker() {
	props.vm.dialogOpen({
		title: "Select the destination for files",
		directory: true,
		multiple: false,
	}).then(async (el) => {
		if (el === null) {
			return;
		}

		await utils.setDownloadPath(props.vm, el as string);
	});
}
</script>

<template>
	<div v-if="vm.settingsOpen" class="absolute z-10 w-full h-full flex justify-center items-center bg-black bg-opacity-25">
		<div class="bg-white dark:bg-neutral-800 rounded-xl shadow-xl p-4 w-[24rem]">
			<div class="flex flex-row justify-between items-center">
				<h3 class="font-medium text-xl">
					Settings
				</h3>
				<div class="btn px-3 rounded-xl active:scale-95 transition duration-150 ease-in-out" @click="emit('close')">
					Close
				</div>
			</div>
			<div class="py-4 flex flex-col">
				<div class="form-control rounded-xl p-3">
					<label class="flex flex-col items-start gap-1">
						<span class="label-text">Device name</span>
						<input
							type="text" :value="vm.hostname" placeholder="Device name"
							@change="(e) => utils.setDeviceName(vm, (e.target as HTMLInputElement).value)"
							class="w-full bg-transparent border border-gray-500 border-opacity-20 rounded-lg px-2 py-1 text-sm focus:outline-none focus:border-green-400">
						<span class="text-xs opacity-60">Shown to nearby devices. Applies fully after a restart.</span>
					</label>
				</div>
				<div class="form-control rounded-xl p-3">
					<span class="label-text block">Appearance</span>
					<span class="text-xs opacity-60 block mb-2">"System" follows your desktop's light/dark theme</span>
					<div class="flex flex-row gap-1 bg-gray-500 bg-opacity-10 rounded-lg p-1">
						<button
							v-for="opt in (['system', 'light', 'dark'] as const)" :key="opt"
							@click="utils.setTheme(vm, opt)"
							class="flex-1 capitalize text-sm rounded-md py-1 transition duration-150 ease-in-out active:scale-95"
							:class="vm.theme === opt ? 'bg-green-200 text-black' : 'hover:bg-gray-500 hover:bg-opacity-10'">
							{{ opt }}
						</button>
					</div>
				</div>
				<div class="form-control rounded-xl p-3">
					<span class="label-text block">Trusted devices</span>
					<span class="text-xs opacity-60 block mb-2">
						Transfers from these devices are accepted automatically — only trust devices you own
					</span>
					<p v-if="trusted.length === 0" class="text-xs opacity-40">
						None yet — tick "Always accept from this device" on an incoming request.
					</p>
					<div v-for="t in trusted" :key="t" class="flex flex-row items-center justify-between text-sm py-1">
						<span class="overflow-hidden text-ellipsis whitespace-nowrap">{{ t }}</span>
						<button
							class="px-2 rounded-md hover:bg-gray-500 hover:bg-opacity-10 active:scale-95 transition duration-150 ease-in-out"
							@click="dropTrusted(t)">
							✕
						</button>
					</div>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-row justify-between items-center gap-3" @click="utils.setAutoStart(vm, !vm.autostart)">
						<span class="flex flex-col">
							<span class="label-text">Start on boot</span>
							<span class="text-xs opacity-60">Launches Open QuickShare automatically when you log in</span>
						</span>
						<input type="checkbox" :checked="vm.autostart" class="checkbox focus:outline-none">
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-row justify-between items-center gap-3" @click="utils.setRealClose(vm, !vm.realclose)">
						<span class="flex flex-col">
							<span class="label-text">Keep running on close</span>
							<span class="text-xs opacity-60">Closing the window keeps sharing active in the system tray</span>
						</span>
						<input type="checkbox" :checked="!vm.realclose" class="checkbox focus:outline-none">
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-row justify-between items-center gap-3" @click="utils.setStartMinimized(vm, !vm.startminimized)">
						<span class="flex flex-col">
							<span class="label-text">Start minimized</span>
							<span class="text-xs opacity-60">Starts hidden in the system tray instead of opening the window</span>
						</span>
						<input type="checkbox" :checked="vm.startminimized" class="checkbox focus:outline-none">
					</label>
				</div>
				<div class="form-control hover:bg-gray-500 hover:bg-opacity-10 rounded-xl p-3">
					<label class="cursor-pointer flex flex-col items-start" @click="openDownloadPicker()">
						<span class="label-text">Change download folder</span>
						<span class="text-xs opacity-60">Where received files are saved</span>
						<span class="overflow-hidden whitespace-nowrap text-ellipsis text-xs opacity-60 max-w-80">
							> {{ vm.downloadPath ?? 'OS User\'s download folder' }}
						</span>
					</label>
				</div>
			</div>
		</div>
	</div>
</template>