<script setup lang="ts">
// Popup 根组件只负责应用级中文语言环境和全局消息提示上下文。
import { dateZhCN, zhCN, NConfigProvider, NMessageProvider } from 'naive-ui';
import PopupContent from './PopupContent.vue';
</script>

<template>
  <n-config-provider :locale="zhCN" :date-locale="dateZhCN">
    <n-message-provider>
      <PopupContent />
    </n-message-provider>
  </n-config-provider>
</template>
