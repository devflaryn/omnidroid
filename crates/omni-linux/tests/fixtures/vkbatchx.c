/* Batching's edge cases (OMNI_VK_BATCH), each checked by what the GPU wrote:
 *
 *  1. a pool reset with a recording's commands not yet sent, then the buffer recorded again;
 *  2. command buffers freed, and a pool destroyed, with commands not yet sent (nothing may reach
 *     the host for them, nothing may crash);
 *  3. a secondary command buffer recorded (batched), executed from a primary whose own batched
 *     commands come before and after vkCmdExecuteCommands;
 *  4. four threads recording at once into their own command buffers, each past a batch's 64 KiB
 *     (a batch sent while recording), then submitted together;
 *  5. a buffer update too big for a batch (64 KiB: sent as it is, after what came before).
 *
 * Words are written with vkCmdUpdateBuffer (its data copied into the batch) and vkCmdFillBuffer,
 * a transfer barrier between writes to the same words. Prints "vkbatchx ok" and exits 0, or the
 * first wrong word and exits 1. */
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define CHECK(step, call)                                         \
    do {                                                          \
        VkResult r_ = (call);                                     \
        if (r_ != VK_SUCCESS) {                                   \
            printf("%s failed %d\n", step, r_);                   \
            return 1;                                             \
        }                                                         \
    } while (0)

enum { WORDS = 64 * 1024 / 4 * 2, THREADS = 4, THREAD_WORDS = 256 };

static VkPhysicalDevice g_pd;
static VkDevice g_device;
static VkQueue g_queue;
static uint32_t g_family;
static VkBuffer g_buffer;
static uint32_t* g_mapped;

static uint32_t memory_type(uint32_t bits, VkMemoryPropertyFlags want) {
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(g_pd, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) return i;
    }
    return UINT32_MAX;
}

static void barrier(VkCommandBuffer cb) {
    VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                          .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT | VK_ACCESS_TRANSFER_READ_BIT};
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
}

/* vkCmdUpdateBuffer of `n` words of `value + i` at word `at`, the source scribbled afterwards. */
static void update(VkCommandBuffer cb, uint32_t at, uint32_t n, uint32_t value) {
    uint32_t words[64];
    for (uint32_t i = 0; i < n; i++) words[i] = value + i;
    vkCmdUpdateBuffer(cb, g_buffer, (VkDeviceSize)at * 4, (VkDeviceSize)n * 4, words);
    memset(words, 0x5a, sizeof words);
    __asm__ volatile("" ::"r"(words) : "memory");
}

static int submit_wait(VkCommandBuffer* cbs, uint32_t n) {
    VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence;
    CHECK("vkCreateFence", vkCreateFence(g_device, &fci, NULL, &fence));
    VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = n, .pCommandBuffers = cbs};
    CHECK("vkQueueSubmit", vkQueueSubmit(g_queue, 1, &si, fence));
    CHECK("vkWaitForFences", vkWaitForFences(g_device, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));
    vkDestroyFence(g_device, fence, NULL);
    return 0;
}

static int expect(const char* what, uint32_t at, uint32_t n, uint32_t value) {
    for (uint32_t i = 0; i < n; i++) {
        if (g_mapped[at + i] != value + i) {
            printf("%s: word %u is %08x, not %08x\n", what, at + i, g_mapped[at + i], value + i);
            return 1;
        }
    }
    return 0;
}

static VkCommandBufferBeginInfo k_begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};

struct job {
    int me;
    VkCommandPool pool;
    VkCommandBuffer cb;
    int failed;
};

static void* record_thread(void* arg) {
    struct job* j = arg;
    VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = j->pool,
                                       .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
    if (vkAllocateCommandBuffers(g_device, &cai, &j->cb) != VK_SUCCESS || vkBeginCommandBuffer(j->cb, &k_begin) != VK_SUCCESS) {
        j->failed = 1;
        return NULL;
    }
    /* ~200 KiB of records: three batches sent while recording. */
    VkViewport vp = {0, 0, 16, 16, 0, 1};
    for (int i = 0; i < 3000; i++) vkCmdSetViewport(j->cb, 0, 1, &vp);
    uint32_t base = 8192 + (uint32_t)j->me * THREAD_WORDS;
    for (uint32_t k = 0; k < THREAD_WORDS; k += 32) update(j->cb, base + k, 32, 0x10000000u * (uint32_t)(j->me + 1) + k);
    for (int i = 0; i < 3000; i++) vkCmdSetViewport(j->cb, 0, 1, &vp);
    if (vkEndCommandBuffer(j->cb) != VK_SUCCESS) j->failed = 1;
    return NULL;
}

int main(void) {
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vkbatchx", .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    VkInstance instance;
    CHECK("vkCreateInstance", vkCreateInstance(&ici, NULL, &instance));
    uint32_t n = 1;
    VkResult er = vkEnumeratePhysicalDevices(instance, &n, &g_pd);
    if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || n == 0) {
        printf("vkEnumeratePhysicalDevices failed %d\n", er);
        return 1;
    }
    uint32_t nq = 16;
    VkQueueFamilyProperties qf[16];
    vkGetPhysicalDeviceQueueFamilyProperties(g_pd, &nq, qf);
    g_family = UINT32_MAX;
    for (uint32_t i = 0; i < nq && g_family == UINT32_MAX; i++) {
        if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) g_family = i;
    }
    float priority = 1.0f;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = g_family, .queueCount = 1,
                                   .pQueuePriorities = &priority};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci};
    CHECK("vkCreateDevice", vkCreateDevice(g_pd, &dci, NULL, &g_device));
    vkGetDeviceQueue(g_device, g_family, 0, &g_queue);
    VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = WORDS * 4,
                              .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT};
    CHECK("vkCreateBuffer", vkCreateBuffer(g_device, &bci, NULL, &g_buffer));
    VkMemoryRequirements req;
    vkGetBufferMemoryRequirements(g_device, g_buffer, &req);
    VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size,
                                .memoryTypeIndex = memory_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)};
    VkDeviceMemory mem;
    CHECK("vkAllocateMemory", vkAllocateMemory(g_device, &mai, NULL, &mem));
    CHECK("vkBindBufferMemory", vkBindBufferMemory(g_device, g_buffer, mem, 0));
    void* mapped;
    CHECK("vkMapMemory", vkMapMemory(g_device, mem, 0, VK_WHOLE_SIZE, 0, &mapped));
    g_mapped = mapped;
    memset(g_mapped, 0, WORDS * 4);

    VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                   .queueFamilyIndex = g_family};
    VkCommandPool pool;
    CHECK("vkCreateCommandPool", vkCreateCommandPool(g_device, &pci, NULL, &pool));
    VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool,
                                       .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};

    /* 1. Pool reset: of an ended recording (it must never run), and in the middle of one (legal;
     * what it had recorded is gone with batching, and NVIDIA's driver keeps it without: not
     * checked); the next recording must run. */
    VkCommandBuffer cb;
    CHECK("vkAllocateCommandBuffers", vkAllocateCommandBuffers(g_device, &cai, &cb));
    CHECK("vkBeginCommandBuffer(1)", vkBeginCommandBuffer(cb, &k_begin));
    update(cb, 0, 16, 0xdead0000u);
    CHECK("vkEndCommandBuffer(1a)", vkEndCommandBuffer(cb));
    CHECK("vkResetCommandPool(1a)", vkResetCommandPool(g_device, pool, 0));
    CHECK("vkBeginCommandBuffer(1b)", vkBeginCommandBuffer(cb, &k_begin));
    update(cb, 4096, 16, 0xdead0001u);
    CHECK("vkResetCommandPool(1b)", vkResetCommandPool(g_device, pool, 0));
    CHECK("vkBeginCommandBuffer(1c)", vkBeginCommandBuffer(cb, &k_begin));
    update(cb, 16, 16, 0x11110000u);
    CHECK("vkEndCommandBuffer(1)", vkEndCommandBuffer(cb));
    if (submit_wait(&cb, 1)) return 1;
    if (expect("pool reset: untouched", 0, 1, 0) || expect("pool reset: recorded again", 16, 16, 0x11110000u)) return 1;

    /* 2. Freed and destroyed with commands pending (not ended). */
    VkCommandBuffer doomed;
    CHECK("vkAllocateCommandBuffers(2)", vkAllocateCommandBuffers(g_device, &cai, &doomed));
    CHECK("vkBeginCommandBuffer(2)", vkBeginCommandBuffer(doomed, &k_begin));
    update(doomed, 32, 16, 0xdead1111u);
    vkFreeCommandBuffers(g_device, pool, 1, &doomed);
    VkCommandPool scratch_pool;
    CHECK("vkCreateCommandPool(2)", vkCreateCommandPool(g_device, &pci, NULL, &scratch_pool));
    VkCommandBufferAllocateInfo scai = cai;
    scai.commandPool = scratch_pool;
    scai.commandBufferCount = 1;
    VkCommandBuffer orphan;
    CHECK("vkAllocateCommandBuffers(2b)", vkAllocateCommandBuffers(g_device, &scai, &orphan));
    CHECK("vkBeginCommandBuffer(2b)", vkBeginCommandBuffer(orphan, &k_begin));
    update(orphan, 48, 16, 0xdead2222u);
    vkDestroyCommandPool(g_device, scratch_pool, NULL);

    /* 3. A secondary inside a primary: primary A, secondary B, primary C, in that order. */
    VkCommandBufferAllocateInfo sai = cai;
    sai.level = VK_COMMAND_BUFFER_LEVEL_SECONDARY;
    VkCommandBuffer secondary;
    CHECK("vkAllocateCommandBuffers(secondary)", vkAllocateCommandBuffers(g_device, &sai, &secondary));
    VkCommandBufferInheritanceInfo inherit = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_INHERITANCE_INFO};
    VkCommandBufferBeginInfo sbegin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .pInheritanceInfo = &inherit};
    CHECK("vkBeginCommandBuffer(secondary)", vkBeginCommandBuffer(secondary, &sbegin));
    update(secondary, 64, 16, 0xbbbb0000u);
    barrier(secondary);
    update(secondary, 96, 16, 0xbbbb1000u);
    CHECK("vkEndCommandBuffer(secondary)", vkEndCommandBuffer(secondary));
    CHECK("vkBeginCommandBuffer(3)", vkBeginCommandBuffer(cb, &k_begin));
    update(cb, 64, 16, 0xaaaa0000u);
    update(cb, 80, 16, 0xaaaa1000u);
    barrier(cb);
    vkCmdExecuteCommands(cb, 1, &secondary);
    barrier(cb);
    update(cb, 96, 16, 0xcccc0000u);
    CHECK("vkEndCommandBuffer(3)", vkEndCommandBuffer(cb));
    if (submit_wait(&cb, 1)) return 1;
    if (expect("secondary over the primary's first", 64, 16, 0xbbbb0000u) || expect("the primary's, kept", 80, 16, 0xaaaa1000u) ||
        expect("the primary's last over the secondary's", 96, 16, 0xcccc0000u)) {
        return 1;
    }
    /* What a freed or destroyed recording had recorded never runs with batching (it was never
     * sent). Without, NVIDIA's driver runs it in the next recording of a reused command buffer:
     * a note, which the gate allows only with batching off. */
    for (uint32_t i = 32; i < 64; i++) {
        if (g_mapped[i] != 0) {
            printf("note: freed/destroyed: word %u is %08x\n", i, g_mapped[i]);
            break;
        }
    }

    /* 4. Four threads recording at once. */
    struct job jobs[THREADS];
    pthread_t threads[THREADS];
    for (int i = 0; i < THREADS; i++) {
        jobs[i] = (struct job){.me = i};
        CHECK("vkCreateCommandPool(4)", vkCreateCommandPool(g_device, &pci, NULL, &jobs[i].pool));
        pthread_create(&threads[i], NULL, record_thread, &jobs[i]);
    }
    VkCommandBuffer recorded[THREADS];
    for (int i = 0; i < THREADS; i++) {
        pthread_join(threads[i], NULL);
        if (jobs[i].failed) {
            printf("thread %d failed to record\n", i);
            return 1;
        }
        recorded[i] = jobs[i].cb;
    }
    if (submit_wait(recorded, THREADS)) return 1;
    for (int i = 0; i < THREADS; i++) {
        for (uint32_t k = 0; k < THREAD_WORDS; k += 32) {
            if (expect("threads", 8192 + (uint32_t)i * THREAD_WORDS + k, 32, 0x10000000u * (uint32_t)(i + 1) + k)) return 1;
        }
        vkDestroyCommandPool(g_device, jobs[i].pool, NULL);
    }

    /* 5. A 64 KiB update (sent as it is) between batched ones. */
    static uint32_t big[16384];
    for (uint32_t i = 0; i < 16384; i++) big[i] = 0x50000000u + i;
    CHECK("vkBeginCommandBuffer(5)", vkBeginCommandBuffer(cb, &k_begin));
    update(cb, 16384, 16, 0x0f0f0000u);
    barrier(cb);
    vkCmdUpdateBuffer(cb, g_buffer, 16384 * 4, sizeof big, big);
    barrier(cb);
    update(cb, 16384 + 16, 16, 0x0e0e0000u);
    CHECK("vkEndCommandBuffer(5)", vkEndCommandBuffer(cb));
    if (submit_wait(&cb, 1)) return 1;
    if (expect("big update", 16384 + 32, 16384 - 32, 0x50000020u) || expect("after the big update", 16384 + 16, 16, 0x0e0e0000u) ||
        expect("big update over the first", 16384, 16, 0x50000000u)) {
        return 1;
    }

    vkFreeCommandBuffers(g_device, pool, 1, &cb);
    vkFreeCommandBuffers(g_device, pool, 1, &secondary);
    vkDestroyCommandPool(g_device, pool, NULL);
    vkUnmapMemory(g_device, mem);
    vkDestroyBuffer(g_device, g_buffer, NULL);
    vkFreeMemory(g_device, mem, NULL);
    vkDestroyDevice(g_device, NULL);
    vkDestroyInstance(instance, NULL);
    printf("vkbatchx ok\n");
    fflush(stdout);
    return 0;
}
