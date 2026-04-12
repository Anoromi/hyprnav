# Else exist specifically for clang
ifeq ($(CXX),g++)
    EXTRA_FLAGS = --no-gnu-unique
else
    EXTRA_FLAGS =
endif

CXXFLAGS ?= -O2
CXXFLAGS += -shared -fPIC -std=c++2b -Wno-narrowing

all:
	$(CXX) $(CXXFLAGS) $(LDFLAGS) $(EXTRA_FLAGS) main.cpp overview.cpp ExpoGesture.cpp OverviewPassElement.cpp PreviewManager.cpp SpawnManager.cpp common.cpp -o hyprexpo.so `pkg-config --cflags pixman-1 libdrm hyprland pangocairo libinput libudev wayland-server xkbcommon` -ljpeg
clean:
	rm ./hyprexpo.so
