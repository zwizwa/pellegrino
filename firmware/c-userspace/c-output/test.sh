set -xe

arm-none-eabi-gcc \
-std=c99 \
-mthumb \
-mcpu=cortex-m4 \
-mhard-float \
-c test.c \
-o test.o

arm-none-eabi-gcc \
--static \
-nostartfiles \
-Tlink.x \
-Wl,-Map=test.map \
-o test.elf \
test.o \
libc_userspace.a

arm-none-eabi-objcopy \
-O binary \
./test.elf \
./test.bin

cp -a test.bin ../../kernel/appbins/

