// The Bluetooth half of wireless CarPlay on macOS.
//
// CarPlay's wireless handshake is not BLE: the phone looks up an iAP2 service
// in SDP and opens an RFCOMM channel to it. CoreBluetooth cannot do either, so
// this is IOBluetooth. The channel is then relayed byte for byte to the
// receiver over a unix socket, because the receiver is a plain Rust program
// with no run loop of its own — and IOBluetooth needs one.
//
// The shape of this follows MacPlay's bridge, which is proven against real
// iPhones: publish two service records, advertise the controller as in-car
// audio/video, poll the paired devices (macOS never tells you when one appears),
// and open the phone's iAP2 channel ourselves when it has one.

#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <IOBluetooth/IOBluetooth.h>
#import <sys/socket.h>
#import <sys/un.h>
#import <unistd.h>

// Service UUIDs. These are CarPlay's own, not ours to choose.
static unsigned char kIapServerUUID[] = {0,0,0,0,0xde,0xca,0xfa,0xde,0xde,0xca,0xde,0xaf,0xde,0xca,0xca,0xff};
static unsigned char kIapClientUUID[] = {0,0,0,0,0xde,0xca,0xfa,0xde,0xde,0xca,0xde,0xaf,0xde,0xca,0xca,0xfe};
static unsigned char kCarPlayUUID[]   = {0xec,0x88,0x43,0x48,0xcd,0x41,0x40,0xa2,0x97,0x27,0x57,0x5d,0x50,0xbf,0x1f,0xd3};

@interface Bridge : NSObject <IOBluetoothRFCOMMChannelDelegate>
@property(nonatomic,strong) IOBluetoothRFCOMMChannel *channel;
@property(nonatomic,strong) IOBluetoothSDPServiceRecord *record;
@property(nonatomic,strong) IOBluetoothSDPServiceRecord *carplayRecord;
@property(nonatomic,strong) IOBluetoothUserNotification *carplayNotification;
@property(nonatomic,strong) IOBluetoothUserNotification *notification;
@property(nonatomic,strong) NSMutableSet *pending;
@property(nonatomic,strong) NSMutableDictionary *results;
@property(nonatomic,assign) IOReturn advertiseResult;
@property(nonatomic,assign) BOOL attaching;
@property(nonatomic,assign) int fd;
@property(nonatomic,strong) NSString *path;
@end

static NSString *targetAddress(void) {
    NSString *target = NSProcessInfo.processInfo.environment[@"CP2AA_TARGET_BT"];
    return [[target stringByReplacingOccurrencesOfString:@"-" withString:@":"] lowercaseString];
}

@implementation Bridge

- (void)attach:(IOBluetoothRFCOMMChannel *)channel {
    NSString *address = [[[channel getDevice] addressString] stringByReplacingOccurrencesOfString:@"-" withString:@":"];
    NSString *target = targetAddress();
    if (target.length && ![target isEqual:address.lowercaseString]) { [channel closeChannel]; return; }
    if (self.attaching || (self.channel == channel && self.fd >= 0)) return;
    if (self.channel && self.channel != channel) { [channel closeChannel]; return; }
    self.attaching = YES;
    self.channel = channel;
    [channel setDelegate:self];

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    strlcpy(addr.sun_path, self.path.UTF8String, sizeof(addr.sun_path));
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        close(fd);
        [channel closeChannel];
        self.channel = nil;
        self.attaching = NO;
        return;
    }
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof(one));
    self.fd = fd;
    self.attaching = NO;

    // The receiver needs our own address to identify with.
    NSString *line = [NSString stringWithFormat:@"%@|%@\n",
        [[IOBluetoothHostController defaultController] addressAsString],
        [[channel getDevice] addressString]];
    NSData *header = [line dataUsingEncoding:NSUTF8StringEncoding];
    send(fd, header.bytes, header.length, 0);
    NSLog(@"cp2aa: RFCOMM connected: %@", [[channel getDevice] name]);

    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        uint8_t buf[1024];
        ssize_t count;
        while ((count = recv(fd, buf, sizeof(buf), 0)) > 0) {
            NSData *data = [NSData dataWithBytes:buf length:count];
            dispatch_sync(dispatch_get_main_queue(), ^{
                if (self.fd != fd) return;
                const uint8_t *bytes = data.bytes;
                NSUInteger offset = 0;
                while (offset < data.length) {
                    UInt16 size = (UInt16)MIN(data.length - offset, [channel getMTU]);
                    if (!size || [channel writeSync:(void *)(bytes + offset) length:size] != kIOReturnSuccess) {
                        shutdown(fd, SHUT_RDWR);
                        break;
                    }
                    offset += size;
                }
            });
        }
        dispatch_async(dispatch_get_main_queue(), ^{
            if (self.fd == fd) {
                self.fd = -1;
                close(fd);
                [channel closeChannel];
                self.channel = nil;
            }
        });
    });
}

- (void)opened:(IOBluetoothUserNotification *)note channel:(IOBluetoothRFCOMMChannel *)channel {
    [self attach:channel];
}

- (void)rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel status:(IOReturn)status {
    [self.pending removeObject:[[channel getDevice] addressString]];
    if (status == kIOReturnSuccess) {
        [self attach:channel];
    } else {
        self.channel = nil;
        NSLog(@"cp2aa: RFCOMM open error: %d", status);
    }
}

- (void)rfcommChannelData:(IOBluetoothRFCOMMChannel *)channel data:(void *)dataPointer length:(size_t)dataLength {
    if (self.fd < 0) return;
    size_t sent = 0;
    while (sent < dataLength) {
        ssize_t n = send(self.fd, (uint8_t *)dataPointer + sent, dataLength - sent, 0);
        if (n <= 0) { shutdown(self.fd, SHUT_RDWR); break; }
        sent += n;
    }
}

- (void)rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)channel {
    if (self.fd >= 0) shutdown(self.fd, SHUT_RDWR);
    self.channel = nil;
}

- (void)sdpQueryComplete:(IOBluetoothDevice *)device status:(IOReturn)status {
    [self.pending removeObject:[device addressString]];
    if (self.channel) return;
    NSString *result;
    if (status != kIOReturnSuccess) {
        result = [NSString stringWithFormat:@"Bluetooth SDP failed for %@: %d", device.name, status];
        if (![self.results[device.addressString] isEqual:result]) NSLog(@"cp2aa: %@", result);
        self.results[device.addressString] = result;
        return;
    }
    IOBluetoothSDPUUID *serviceUUID = [IOBluetoothSDPUUID uuidWithBytes:kIapClientUUID length:16];
    IOBluetoothSDPServiceRecord *record = [device getServiceRecordForUUID:serviceUUID];
    BluetoothRFCOMMChannelID channelID = 0;
    if (record && [record getRFCOMMChannelID:&channelID] == kIOReturnSuccess) {
        NSLog(@"cp2aa: iAP2 service found on %@ channel=%u", device.name, channelID);
        IOBluetoothRFCOMMChannel *channel = nil;
        IOReturn opened = [device openRFCOMMChannelAsync:&channel withChannelID:channelID delegate:self];
        if (opened == kIOReturnSuccess) {
            self.channel = channel;
            // A channel that never comes up usually means stale service records
            // on the phone; drop it and ask again.
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
                if (self.channel == channel && self.fd < 0) {
                    NSLog(@"cp2aa: RFCOMM timed out; refreshing iPhone service records");
                    [channel closeChannel];
                    self.channel = nil;
                    self.results[device.addressString] = @"refresh";
                    [self.pending addObject:device.addressString];
                    [device performSDPQuery:self uuids:@[serviceUUID]];
                }
            });
        } else {
            NSLog(@"cp2aa: RFCOMM request failed: %d", opened);
        }
    } else {
        result = [NSString stringWithFormat:@"Bluetooth iAP2 service unavailable for %@; open CarPlay settings on iPhone", device.name];
        if (![self.results[device.addressString] isEqual:result]) NSLog(@"cp2aa: %@", result);
        self.results[device.addressString] = result;
    }
}

- (void)scan {
    if (self.channel) return;
    NSString *target = targetAddress();
    for (IOBluetoothDevice *device in [IOBluetoothDevice pairedDevices]) {
        NSString *address = [device.addressString stringByReplacingOccurrencesOfString:@"-" withString:@":"];
        if (target.length && ![target isEqual:address.lowercaseString]) continue;
        if (![[device name].lowercaseString containsString:@"iphone"]) continue;
        if ([self.pending containsObject:device.addressString]) continue;
        [self.pending addObject:device.addressString];
        IOBluetoothSDPUUID *uuid = [IOBluetoothSDPUUID uuidWithBytes:kIapClientUUID length:16];
        if (![self.results[device.addressString] isEqual:@"refresh"] && [device getServiceRecordForUUID:uuid]) {
            [self sdpQueryComplete:device status:kIOReturnSuccess];
            continue;
        }
        if (!self.results[device.addressString]) {
            NSLog(@"cp2aa: querying iPhone Bluetooth service: %@", device.name);
            self.results[device.addressString] = @"querying";
        }
        IOReturn status = [device performSDPQuery:self uuids:@[uuid]];
        if (status != kIOReturnSuccess) {
            [self.pending removeObject:device.addressString];
            [self sdpQueryComplete:device status:status];
        } else {
            NSString *pending = device.addressString;
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 25 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
                if ([self.pending containsObject:pending]) {
                    NSLog(@"cp2aa: Bluetooth SDP timed out; confirm pairing on both devices");
                }
            });
        }
    }
}

- (void)start {
    self.fd = -1;
    self.pending = [NSMutableSet set];
    self.results = [NSMutableDictionary dictionary];
    NSLog(@"cp2aa: paired Bluetooth devices: %lu", (unsigned long)[IOBluetoothDevice pairedDevices].count);

    // Apple's public API can advertise us as in-car audio/video, but only for a
    // while at a time — the controller restores its real class afterwards, so
    // it has to be renewed. Without this the phone does not treat us as a car.
    self.advertiseResult = [[IOBluetoothHostController defaultController] setClassOfDevice:0x200408 forTimeInterval:120];
    NSLog(@"cp2aa: CarPlay device class result: %d", self.advertiseResult);
    if (self.advertiseResult == kIOReturnSuccess) {
        [NSTimer scheduledTimerWithTimeInterval:60 repeats:YES block:^(NSTimer *timer) {
            IOReturn result = [[IOBluetoothHostController defaultController] setClassOfDevice:0x200408 forTimeInterval:120];
            if (result != self.advertiseResult) NSLog(@"cp2aa: CarPlay device class result: %d", result);
            self.advertiseResult = result;
        }];
    }

    self.record = [IOBluetoothSDPServiceRecord publishedServiceRecordWithDictionary:@{
        @"0001": @[[IOBluetoothSDPUUID uuidWithBytes:kIapServerUUID length:16]],
        @"0004": @[@[[IOBluetoothSDPUUID uuid16:0x0100]],
                   @[[IOBluetoothSDPUUID uuid16:0x0003],
                     @{@"DataElementType": @1, @"DataElementSize": @1, @"DataElementValue": @18}]],
        @"0100": @"cp2aa",
        @"LocalAttributes": @{@"Persistent": @NO}
    }];

    self.carplayRecord = [IOBluetoothSDPServiceRecord publishedServiceRecordWithDictionary:@{
        @"0001": @[[IOBluetoothSDPUUID uuidWithBytes:kCarPlayUUID length:16]],
        @"0004": @[@[[IOBluetoothSDPUUID uuid16:0x0100]],
                   @[[IOBluetoothSDPUUID uuid16:0x0003],
                     @{@"DataElementType": @1, @"DataElementSize": @1, @"DataElementValue": @19}]],
        @"0005": @[[IOBluetoothSDPUUID uuid16:0x1002]],
        @"0100": @"cp2aa CarPlay"
    }];

    BluetoothRFCOMMChannelID cpChannel = 0;
    if (self.carplayRecord && [self.carplayRecord getRFCOMMChannelID:&cpChannel] == kIOReturnSuccess) {
        self.carplayNotification = [IOBluetoothRFCOMMChannel
            registerForChannelOpenNotifications:self
                                       selector:@selector(opened:channel:)
                                  withChannelID:cpChannel
                                      direction:kIOBluetoothUserNotificationChannelDirectionIncoming];
    }
    BluetoothRFCOMMChannelID channelID = 0;
    if (self.record && [self.record getRFCOMMChannelID:&channelID] == kIOReturnSuccess) {
        self.notification = [IOBluetoothRFCOMMChannel
            registerForChannelOpenNotifications:self
                                       selector:@selector(opened:channel:)
                                  withChannelID:channelID
                                      direction:kIOBluetoothUserNotificationChannelDirectionIncoming];
        NSLog(@"cp2aa: Bluetooth service registered on channel %u", channelID);
    } else {
        NSLog(@"cp2aa: Bluetooth service registration failed");
    }

    [NSTimer scheduledTimerWithTimeInterval:12 repeats:YES block:^(NSTimer *timer) { [self scan]; }];
    [self scan];
}

@end

int main(int argc, const char **argv) { @autoreleasepool {
    if (argc != 2) return 2;
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    Bridge *bridge = [Bridge new];
    bridge.path = [NSString stringWithUTF8String:argv[1]];
    // IOBluetooth initialises CoreBluetooth synchronously and waits for
    // callbacks, so bootstrap off the main thread and let AppKit service the
    // run loop. Doing this the other way round deadlocks.
    NSLog(@"cp2aa: Bluetooth bridge starting");
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        [IOBluetoothDevice pairedDevices];
        dispatch_async(dispatch_get_main_queue(), ^{ [bridge start]; });
    });
    [NSApp run];
} return 0; }
