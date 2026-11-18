package com.example.web;

import java.util.List;
import org.springframework.web.bind.annotation.*;

interface Named {
    String name();
}

class Owner implements Named, Comparable<Owner> {
    public String name() {
        return "owner";
    }

    public int compareTo(Owner other) {
        return name().compareTo(other.name());
    }
}

@RestController
@RequestMapping("/owners")
public class OwnerController {
    private final OwnerRepository owners;

    OwnerController(OwnerRepository owners) {
        this.owners = owners;
    }

    @GetMapping
    public List<Owner> list() {
        return owners.findAll();
    }

    @GetMapping("/{ownerId}")
    public Owner show(@PathVariable int ownerId) {
        return owners.findById(ownerId).orElseThrow();
    }

    @PostMapping(value = "/{ownerId}/pets", consumes = "application/json")
    public void addPet(@PathVariable("ownerId") int ownerId, @RequestBody Pet pet) {
        owners.findById(ownerId).ifPresent(o -> o.addPet(pet));
    }

    @RequestMapping(path = "/{ownerId:[0-9]+}/visits", method = {RequestMethod.PUT, RequestMethod.PATCH})
    public void visits(@PathVariable int ownerId) {
        owners.touch(ownerId);
    }

    @Override
    public String toString() {
        return String.valueOf(owners.count());
    }
}

@RestController
class HealthController {
    @DeleteMapping("/cache")
    public void clear() {
        Cache.clear();
    }
}
