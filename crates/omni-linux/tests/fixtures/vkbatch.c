/* Batching's gate fixture (OMNI_VK_BATCH), and what a forwarded command costs from the guest.
 *
 * 1. Records into one command buffer, the way an engine does: a clear, a copy and a buffer update
 *    whose arguments point at the caller's stack, each scribbled over right after the call -- a
 *    batch must carry its own copy, not the caller's memory -- and barriers between (not batched:
 *    the batch goes first). Submits, waits, and checks the pixels and the updated words.
 * 2. Times 200,000 vkCmdSetViewport into a recording command buffer (ended and begun again every
 *    5,000, never submitted): the whole path -- the driver's stub, the system call (or the batch),
 *    the host's half and the host driver.
 *
 * Prints "vkbatch ok <ns per vkCmdSetViewport> <device name>" and exits 0, or names the step that
 * failed and exits 1. */
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <vulkan/vulkan.h>

#define CHECK(step, call)                                         \
    do {                                                          \
        VkResult r_ = (call);                                     \
        if (r_ != VK_SUCCESS) {                                   \
            printf("%s failed %d\n", step, r_);                   \
            return 1;                                             \
        }                                                         \
    } while (0)

enum { W = 16, H = 16, WORDS = 4 };

static uint32_t memory_type(VkPhysicalDevice pd, uint32_t bits, VkMemoryPropertyFlags want) {
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) return i;
    }
    return UINT32_MAX;
}

/* Overwrite what a command pointed at, as the caller's next use of its stack would. */
static void scribble(void* p, size_t n) {
    memset(p, 0x5a, n);
    __asm__ volatile("" ::"r"(p) : "memory");
}

static double now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec * 1e9 + (double)t.tv_nsec;
}

int main(void) {
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vkbatch", .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    VkInstance instance;
    CHECK("vkCreateInstance", vkCreateInstance(&ici, NULL, &instance));
    uint32_t n = 1;
    VkPhysicalDevice pd;
    VkResult er = vkEnumeratePhysicalDevices(instance, &n, &pd);
    if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || n == 0) {
        printf("vkEnumeratePhysicalDevices failed %d (%u)\n", er, n);
        return 1;
    }
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    uint32_t nq = 16;
    VkQueueFamilyProperties qf[16];
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qf);
    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < nq && family == UINT32_MAX; i++) {
        if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) family = i;
    }
    if (family == UINT32_MAX) {
        printf("no graphics queue\n");
        return 1;
    }
    float priority = 1.0f;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = family, .queueCount = 1,
                                   .pQueuePriorities = &priority};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci};
    VkDevice device;
    CHECK("vkCreateDevice", vkCreateDevice(pd, &dci, NULL, &device));
    VkQueue queue;
    vkGetDeviceQueue(device, family, 0, &queue);

    VkImageCreateInfo imci = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
                              .extent = {W, H, 1}, .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
                              .tiling = VK_IMAGE_TILING_OPTIMAL,
                              .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                              .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED};
    VkImage image;
    CHECK("vkCreateImage", vkCreateImage(device, &imci, NULL, &image));
    VkMemoryRequirements req;
    vkGetImageMemoryRequirements(device, image, &req);
    VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size,
                                .memoryTypeIndex = memory_type(pd, req.memoryTypeBits, 0)};
    VkDeviceMemory image_mem;
    CHECK("vkAllocateMemory(image)", vkAllocateMemory(device, &mai, NULL, &image_mem));
    CHECK("vkBindImageMemory", vkBindImageMemory(device, image, image_mem, 0));

    /* The pixels, then the words vkCmdUpdateBuffer writes. */
    VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = W * H * 4 + WORDS * 4,
                              .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT};
    VkBuffer buffer;
    CHECK("vkCreateBuffer", vkCreateBuffer(device, &bci, NULL, &buffer));
    vkGetBufferMemoryRequirements(device, buffer, &req);
    mai.allocationSize = req.size;
    mai.memoryTypeIndex = memory_type(pd, req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    VkDeviceMemory buffer_mem;
    CHECK("vkAllocateMemory(buffer)", vkAllocateMemory(device, &mai, NULL, &buffer_mem));
    CHECK("vkBindBufferMemory", vkBindBufferMemory(device, buffer, buffer_mem, 0));

    VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                   .queueFamilyIndex = family};
    VkCommandPool pool;
    CHECK("vkCreateCommandPool", vkCreateCommandPool(device, &pci, NULL, &pool));
    VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool,
                                       .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
    VkCommandBuffer cb;
    CHECK("vkAllocateCommandBuffers", vkAllocateCommandBuffers(device, &cai, &cb));
    VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};

    /* 1. Order and copies. */
    CHECK("vkBeginCommandBuffer", vkBeginCommandBuffer(cb, &begin));
    VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    VkImageMemoryBarrier b = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                              .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                              .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                              .image = image, .subresourceRange = range};
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b);
    {
        VkClearColorValue color = {.float32 = {0.25f, 0.5f, 0.75f, 1.0f}};
        VkImageSubresourceRange clear_range = range;
        vkCmdClearColorImage(cb, image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &color, 1, &clear_range);
        scribble(&color, sizeof color);
        scribble(&clear_range, sizeof clear_range);
    }
    {
        uint32_t words[WORDS] = {0x11111111u, 0x22222222u, 0x33333333u, 0x44444444u};
        vkCmdUpdateBuffer(cb, buffer, W * H * 4, sizeof words, words);
        scribble(words, sizeof words);
    }
    b.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    b.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
    b.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    b.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b);
    {
        VkBufferImageCopy copy = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1}, .imageExtent = {W, H, 1}};
        vkCmdCopyImageToBuffer(cb, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, buffer, 1, &copy);
        scribble(&copy, sizeof copy);
    }
    CHECK("vkEndCommandBuffer", vkEndCommandBuffer(cb));
    VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence;
    CHECK("vkCreateFence", vkCreateFence(device, &fci, NULL, &fence));
    VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
    CHECK("vkQueueSubmit", vkQueueSubmit(queue, 1, &si, fence));
    CHECK("vkWaitForFences", vkWaitForFences(device, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));
    void* mapped;
    CHECK("vkMapMemory", vkMapMemory(device, buffer_mem, 0, VK_WHOLE_SIZE, 0, &mapped));
    const uint8_t* px = mapped;
    for (int i = 0; i < W * H; i++) {
        const uint8_t* q = px + i * 4;
        int ok = q[0] >= 63 && q[0] <= 65 && q[1] >= 127 && q[1] <= 129 && q[2] >= 190 && q[2] <= 192 && q[3] == 255;
        if (!ok) {
            printf("pixel %d is %u %u %u %u\n", i, q[0], q[1], q[2], q[3]);
            return 1;
        }
    }
    const uint32_t* words = (const uint32_t*)(px + W * H * 4);
    if (words[0] != 0x11111111u || words[1] != 0x22222222u || words[2] != 0x33333333u || words[3] != 0x44444444u) {
        printf("updated words are %08x %08x %08x %08x\n", words[0], words[1], words[2], words[3]);
        return 1;
    }
    vkUnmapMemory(device, buffer_mem);

    /* 2. The cost of a cheap command. */
    enum { CALLS = 200000, PER_BUFFER = 5000 };
    VkViewport vp = {0.0f, 0.0f, (float)W, (float)H, 0.0f, 1.0f};
    double took = 0;
    for (int round = 0; round < CALLS / PER_BUFFER; round++) {
        CHECK("vkBeginCommandBuffer(bench)", vkBeginCommandBuffer(cb, &begin));
        double t0 = now_ns();
        for (int i = 0; i < PER_BUFFER; i++) {
            vp.x = (float)(i & 7);
            vkCmdSetViewport(cb, 0, 1, &vp);
        }
        CHECK("vkEndCommandBuffer(bench)", vkEndCommandBuffer(cb));
        took += now_ns() - t0;
    }
    printf("vkbatch ok %.1f %s\n", took / CALLS, props.deviceName);

    vkDestroyFence(device, fence, NULL);
    vkFreeCommandBuffers(device, pool, 1, &cb);
    vkDestroyCommandPool(device, pool, NULL);
    vkDestroyBuffer(device, buffer, NULL);
    vkFreeMemory(device, buffer_mem, NULL);
    vkDestroyImage(device, image, NULL);
    vkFreeMemory(device, image_mem, NULL);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(instance, NULL);
    fflush(stdout);
    return 0;
}
