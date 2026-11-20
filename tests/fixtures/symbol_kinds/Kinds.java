package kinds;

import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;

enum Color {
    RED,
    GREEN
}

record Point(int x, int y) {
    Point {
        if (x < 0) {
            throw new IllegalArgumentException();
        }
    }
}

@Retention(RetentionPolicy.RUNTIME)
@interface Marker {
    String value() default "";
}

interface Limits {
    int MAX_ITEMS = 10;
}

class Account {
    public static final String PREFIX = "acct-";
    final static int VERSION = 2;
    private String owner, currency;

    Account(String owner) {
        int local = 0;
        this.owner = owner;
    }
}
