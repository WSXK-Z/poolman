<script setup lang="ts">
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
