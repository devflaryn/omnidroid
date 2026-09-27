public class Hello {
    public static void main(String[] args) {
        long sum = 0;
        for (int i = 1; i <= 1000; i++) sum += i;
        System.out.println("Hello from real ART on omnidroid, sum=" + sum + ", " + System.getProperty("java.vm.version"));
    }
}
