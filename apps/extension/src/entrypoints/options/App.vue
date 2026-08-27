<script setup lang="ts">
import {
  darkTheme,
  dateZhCN,
  zhCN,
  NConfigProvider,
  NMessageProvider,
  NDialogProvider,
  NTabPane,
  NTabs,
} from 'naive-ui';
import { ref } from 'vue';
import GeneralSettings from './components/GeneralSettings.vue';
import ProviderConfig from './components/ProviderConfig.vue';
import LogViewer from './components/LogViewer.vue';

const dark = ref(false);
const activeTab = ref('general');
</script>

<template>
  <n-config-provider :theme="dark ? darkTheme : null" :locale="zhCN" :date-locale="dateZhCN">
    <n-message-provider>
      <n-dialog-provider>
        <div class="options-root">
          <div class="options-header">AI Gateway 设置</div>
          <n-card>
            <n-tabs v-model:value="activeTab" type="line" animated>
              <n-tab-pane name="general" tab="常规设置">
                <GeneralSettings />
              </n-tab-pane>
              <n-tab-pane name="providers" tab="平台配置">
                <ProviderConfig />
              </n-tab-pane>
              <n-tab-pane name="logs" tab="日志">
                <LogViewer />
              </n-tab-pane>
            </n-tabs>
          </n-card>
        </div>
      </n-dialog-provider>
    </n-message-provider>
  </n-config-provider>
</template>
