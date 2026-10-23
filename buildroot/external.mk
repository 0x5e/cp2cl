# Every package in this tree, wherever it sits under package/.
include $(sort $(wildcard $(BR2_EXTERNAL_CP2CL_PATH)/package/*/*.mk))
